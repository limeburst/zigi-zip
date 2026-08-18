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
    pub r2: R2Config,
}

/// Cloudflare R2 credentials, addressed through the S3-compatible API.
#[derive(Debug, Deserialize, Clone)]
pub struct R2Config {
    /// Jurisdiction-specific S3 endpoint, e.g. `https://<account>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// R2 ignores the region but SigV4 requires one; "auto" is Cloudflare's.
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_base_url() -> String {
    "https://zigi.zip".into()
}

fn default_region() -> String {
    "auto".into()
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
        std::fs::create_dir_all(cfg.tmp_dir())?;
        Ok(cfg)
    }

    /// Scratch space for in-flight uploads. Bodies are spooled here so we can
    /// hash them and know their length before handing them to R2; nothing in
    /// this directory outlives the request that created it.
    pub fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join("tmp")
    }

    /// Pre-R2 blob directory. Only the storage migration reads this.
    pub fn legacy_files_dir(&self) -> PathBuf {
        self.data_dir.join("files")
    }
}
