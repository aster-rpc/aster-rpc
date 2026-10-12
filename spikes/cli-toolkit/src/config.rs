//! `~/.aster/config.toml`. No compatibility with the Python CLI is required.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Profile {
    pub root_pubkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

pub fn path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

pub fn load(home: &Path) -> Result<Config> {
    let path = path(home);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Atomic replace; owner-only permissions on Unix.
pub fn save(home: &Path, config: &Config) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let path = path(home);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
