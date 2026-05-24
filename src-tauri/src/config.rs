use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// All user-configurable R2 credentials and settings.
/// Stored as plain JSON in the OS app-data dir.
/// Sensitive fields (keys) live only on disk — never in memory longer than needed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub account_id: String,

    #[serde(default = "default_bucket")]
    pub bucket: String,

    #[serde(default)]
    pub access_key_id: String,

    #[serde(default)]
    pub secret_access_key: String,

    /// e.g. "https://pub-45635e12296943dd94ce39106dfc2555.r2.dev"
    #[serde(default)]
    pub public_url_base: String,
}

fn default_bucket() -> String {
    "r2share".to_string()
}

impl Config {
    /// True when the minimum fields required for R2 operations are filled.
    pub fn is_configured(&self) -> bool {
        !self.account_id.is_empty()
            && !self.access_key_id.is_empty()
            && !self.secret_access_key.is_empty()
            && !self.public_url_base.is_empty()
    }

    /// Load config from disk; returns Default if missing or corrupt.
    pub fn load(data_dir: &PathBuf) -> Self {
        let path = data_dir.join("config.json");
        if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// Persist config to disk.
    pub fn save(&self, data_dir: &PathBuf) -> Result<(), String> {
        fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        let path = data_dir.join("config.json");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&path, json).map_err(|e| e.to_string())
    }
}
