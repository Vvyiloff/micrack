//! App settings + plugin scan cache, persisted as JSON in %APPDATA%\micrack.
//! MVP scan is lazy: enumerate plugin files, use cached metadata when path+mtime
//! match, otherwise file stem (full probe happens on add).

use crate::api::{EngineConfig, PluginEntry};
use crate::plugins::{PluginInfo, format_from_path};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub fn app_dir() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into())).join("micrack")
}

#[derive(Serialize, Deserialize, Default)]
pub struct Settings {
    pub last_config: Option<EngineConfig>,
    pub input_gain_db: f32,
    pub master_gain_db: f32,
    pub extra_dirs: Vec<String>,
    /// Headphones playback device for the monitor button; None = Windows default.
    #[serde(default)]
    pub monitor_device: Option<String>,
    /// Last ASIO devices that actually started, per side. On a host switch cpal
    /// marks an arbitrary first driver as default; the GUI uses this to re-pick
    /// the right one (e.g. Voicemeeter Insert, not the AUX cable).
    #[serde(default)]
    pub last_asio_input: Option<String>,
    #[serde(default)]
    pub last_asio_output: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub path: String,
    pub mtime: u64,
    pub info: PluginInfo,
}

fn read_json<T: serde::de::DeserializeOwned + Default>(file: &str) -> T {
    std::fs::read_to_string(app_dir().join(file))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(file: &str, v: &T) -> anyhow::Result<()> {
    let dir = app_dir();
    std::fs::create_dir_all(&dir)?;
    // Atomic (M1): settings.json carries last_config for auto-start; a torn
    // write must not silently drop it.
    crate::preset::atomic_write(&dir.join(file), serde_json::to_string_pretty(v)?.as_bytes())
}

pub fn load_settings() -> Settings {
    read_json("settings.json")
}

pub fn save_settings(s: &Settings) -> anyhow::Result<()> {
    write_json("settings.json", s)
}

pub fn load_cache() -> Vec<CacheEntry> {
    read_json("plugin_cache.json")
}

pub fn save_cache(c: &[CacheEntry]) -> anyhow::Result<()> {
    write_json("plugin_cache.json", &c)
}

pub fn default_dirs() -> Vec<String> {
    vec![
        r"C:\Program Files\Common Files\VST3".into(),
        r"C:\Program Files\VSTPlugins".into(),
        r"C:\Program Files\Steinberg\VSTPlugins".into(),
    ]
}

pub fn file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn scan(dirs: &[String], cache: &[CacheEntry], mut progress: impl FnMut(usize, usize, &str)) -> Vec<PluginEntry> {
    let mut files: Vec<String> = Vec::new();
    for dir in dirs {
        let p = Path::new(dir);
        if p.is_file() {
            files.push(dir.clone());
            continue;
        }
        // Recursive walk: vendors install into subfolders (e.g. VSTPlugins\ReaPlugs).
        // .vst3 bundles are listed as-is, never walked into; depth cap guards
        // junction/symlink cycles.
        let mut stack = vec![(p.to_path_buf(), 0usize)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let path = e.path();
                let ext = path.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                if ext == "vst3" {
                    files.push(path.to_string_lossy().into_owned());
                } else if path.is_dir() {
                    if depth < 12 {
                        stack.push((path, depth + 1));
                    }
                } else if ext == "dll" {
                    files.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }
    files.sort();
    files.dedup();
    let total = files.len();
    files
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            progress(i + 1, total, &path);
            let cached = cache.iter().find(|c| c.path == path && c.mtime == file_mtime(Path::new(&path)));
            match cached {
                Some(c) => PluginEntry {
                    id: path.clone(),
                    name: c.info.name.clone(),
                    vendor: c.info.vendor.clone(),
                    format: c.info.format,
                    path,
                    has_gui: c.info.has_gui,
                },
                None => PluginEntry {
                    name: Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone()),
                    vendor: String::new(),
                    format: format_from_path(&path),
                    id: path.clone(),
                    path,
                    has_gui: false,
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_nested_plugins_recursively() {
        let root = std::env::temp_dir().join(format!("micrack-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("ReaPlugs").join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("top.dll"), b"dummy").unwrap();
        std::fs::write(nested.join("deep.dll"), b"dummy").unwrap();
        std::fs::write(nested.join("flat.vst3"), b"dummy").unwrap();
        std::fs::write(root.join("readme.txt"), b"not a plugin").unwrap();
        // A .vst3 bundle DIRECTORY is one plugin; its inner dll must not be listed.
        let bundle = root.join("DirBundle.vst3");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents").join("inner.dll"), b"dummy").unwrap();

        let entries = scan(&[root.to_string_lossy().into_owned()], &[], |_, _, _| {});
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(entries.len(), 4, "unexpected entries: {paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("top.dll")));
        assert!(paths.iter().any(|p| p.ends_with("deep.dll")), "nested DLL missed: {paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("flat.vst3")));
        assert!(paths.iter().any(|p| p.ends_with("DirBundle.vst3")));
        assert!(!paths.iter().any(|p| p.ends_with("inner.dll")), "walked into a .vst3 bundle");
        assert!(!paths.iter().any(|p| p.ends_with("readme.txt")));
        let _ = std::fs::remove_dir_all(&root);
    }
}