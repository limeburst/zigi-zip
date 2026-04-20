use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
    pub data_dir: PathBuf,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

fn default_base_url() -> String {
    "https://zigi.zip".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&raw)?;
        if let Some(s) = cfg.data_dir.to_str() {
            if let Some(rest) = s.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    cfg.data_dir = PathBuf::from(home).join(rest);
                }
            }
        }
        std::fs::create_dir_all(cfg.files_dir())?;
        Ok(cfg)
    }

    pub fn files_dir(&self) -> PathBuf {
        self.data_dir.join("files")
    }
}
