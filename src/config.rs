//! Settings, kept in %APPDATA%\Lossless Link\config.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerRef {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub latency_ms: u32,
    pub speaker: Option<SpeakerRef>,
    /// Endpoint id of the output Windows plays into; nothing audible should be connected to it.
    pub capture_device: Option<String>,
    /// Highest speaker volume the Windows volume keys can set.
    pub max_volume: u32,
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { latency_ms: 100, speaker: None, capture_device: None, max_volume: 70, auto_update: true }
    }
}

/// %APPDATA%\Lossless Link or %LOCALAPPDATA%\Lossless Link, depending on `var`.
pub fn app_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Lossless Link")
}

impl Config {
    fn path() -> PathBuf {
        app_dir("APPDATA").join("config.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let written = serde_json::to_vec_pretty(self).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&path, b)?));
        if let Err(e) = written {
            log::warn!("saving settings: {e:#}");
        }
    }
}
