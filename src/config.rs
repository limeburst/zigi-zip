use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
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
        Ok(toml::from_str(&raw)?)
    }
}
