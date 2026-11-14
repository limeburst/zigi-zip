use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub r2: R2Config,
    /// Local disk cache of recently uploaded and served files. Off when absent.
    #[serde(default)]
    pub cache: Option<CacheConfig>,
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

#[derive(Debug, Deserialize, Clone)]
pub struct CacheConfig {
    /// Where cached files live. A leading `~/` means the home directory.
    pub dir: PathBuf,
    /// Total size the cache may grow to; least recently used files go first.
    #[serde(default = "default_cache_max_size_mb")]
    pub max_size_mb: u64,
    /// Files larger than this are never cached, and are always fetched from R2.
    #[serde(default = "default_cache_max_file_mb")]
    pub max_file_mb: u64,
}

fn default_base_url() -> String {
    "https://zigi.zip".into()
}

fn default_region() -> String {
    "auto".into()
}

fn default_cache_max_size_mb() -> u64 {
    1024
}

fn default_cache_max_file_mb() -> u64 {
    100
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&raw)?;
        if let Some(cache) = cfg.cache.as_mut() {
            if let Some(rest) = cache.dir.to_str().and_then(|s| s.strip_prefix("~/")) {
                if let Some(home) = std::env::var_os("HOME") {
                    cache.dir = PathBuf::from(home).join(rest);
                }
            }
        }
        Ok(cfg)
    }
}
