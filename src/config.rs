//! On-disk locations and persistent settings.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("GREENLIGHT_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("io.github", "greenlight-rs", "greenlight")
        .map(|d| d.config_dir().to_path_buf())
        .ok_or_else(|| anyhow!("cannot determine config directory; set GREENLIGHT_CONFIG_DIR"))
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("tokens.json"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// IP address sent as `X-Forwarded-For` to pick a different xCloud region.
    pub force_region_ip: Option<String>,
    /// Game language/locale (e.g. `en-US`).
    pub locale: Option<String>,
    /// Preferred xCloud region name (e.g. `WestEurope`).
    pub xcloud_region: Option<String>,
}

impl Settings {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("settings.json"))
    }

    pub fn load() -> Result<Self> {
        match std::fs::read_to_string(Self::path()?) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(_) => Ok(Self::default()),
        }
    }
}
