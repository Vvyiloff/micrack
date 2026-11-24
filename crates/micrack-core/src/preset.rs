//! Versioned JSON preset (version 1). Plugin states are base64 blobs.

use crate::api::HostId;
use crate::plugins::PluginFormat;
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Preset {
    pub version: u32,
    pub name: String,
    pub engine: PresetEngine,
    pub input: PresetIo,
    pub output: PresetIo,
    pub chain: Vec<PresetSlot>,
}

#[derive(Serialize, Deserialize)]
pub struct PresetEngine {
    pub sample_rate: u32,
    pub block_size: usize,
}

#[derive(Serialize, Deserialize)]
pub struct PresetIo {
    pub host: HostId,
    pub device: String,
    pub gain_db: f32,
}

#[derive(Serialize, Deserialize)]
pub struct PresetSlot {
    pub format: PluginFormat,
    pub path: String,
    pub uid: String,
    pub bypass: bool,
    pub state_b64: String,
}

pub fn encode_state(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn decode_state(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).context("preset state base64")
}

/// Atomic file write (M1): temp file + sync + rename, so a kill mid-write can
/// never leave a truncated last-session.json (which would auto-start the raw
/// mic at a hot gain). Same-directory temp keeps the rename on one volume.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "preset".into())
    ));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("sync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

pub fn write(path: &Path, p: &Preset) -> Result<()> {
    let json = serde_json::to_string_pretty(p).context("preset serialize")?;
    atomic_write(path, json.as_bytes()).with_context(|| format!("write preset {}", path.display()))
}

pub fn read(path: &Path) -> Result<Preset> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read preset {}", path.display()))?;
    let p: Preset = serde_json::from_str(&s).context("preset parse")?;
    if p.version != VERSION {
        bail!("unsupported preset version {}", p.version);
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::HostId;

    fn demo() -> Preset {
        Preset {
            version: VERSION,
            name: "roundtrip".into(),
            engine: PresetEngine { sample_rate: 48000, block_size: 512 },
            input: PresetIo { host: HostId::Wasapi, device: "in".into(), gain_db: 3.0 },
            output: PresetIo { host: HostId::Wasapi, device: "out".into(), gain_db: -2.0 },
            chain: vec![PresetSlot {
                format: PluginFormat::Vst2,
                path: "p.dll".into(),
                uid: "u".into(),
                bypass: true,
                state_b64: encode_state(&[1, 2, 3]),
            }],
        }
    }

    #[test]
    fn atomic_write_roundtrip_and_overwrite() {
        let dir = std::env::temp_dir().join(format!("micrack-preset-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        write(&path, &demo()).unwrap();
        let p = read(&path).unwrap();
        assert_eq!(p.chain.len(), 1);
        assert_eq!(decode_state(&p.chain[0].state_b64).unwrap(), vec![1, 2, 3]);
        // Overwrite an existing file (rename must replace it) and leave no temp.
        write(&path, &demo()).unwrap();
        assert!(read(&path).is_ok());
        assert!(!path.with_file_name("session.json.tmp").exists(), "temp file left behind");
        // A pre-existing truncated file is replaced atomically, never appended.
        std::fs::write(&path, b"{\"version\":").unwrap();
        write(&path, &demo()).unwrap();
        assert_eq!(read(&path).unwrap().name, "roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
    }
}