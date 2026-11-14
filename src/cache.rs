//! Local disk cache of R2 objects.
//!
//! Object keys are SHA-256 content hashes, so a key always names the same
//! bytes and a cached copy can never go stale: the only thing to keep in step
//! with R2 is deletion. Files are kept as `<dir>/<key>`, and the least
//! recently used ones are dropped once the cache passes its size limit.
//!
//! Uploads are spooled straight into the cache directory, so keeping one is a
//! rename rather than a copy. A miss is filled while the file streams to the
//! client, and only kept if the bytes match their hash.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{fs::File, io::AsyncWriteExt};

use crate::{config::CacheConfig, shortcode};

/// Prefixes of in-progress files; anything with them is left over from a
/// crash when the cache opens, and is removed.
const SPOOL_PREFIX: &str = "upload-";
const FILL_PREFIX: &str = "fill-";

#[derive(Clone)]
pub struct DiskCache {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    max_file_bytes: u64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    total: u64,
    /// Monotonic use counter; a higher `last_used` is more recent.
    clock: u64,
    /// Keys being filled from R2 right now, so two requests don't both fill.
    filling: HashSet<String>,
}

struct Entry {
    size: u64,
    last_used: u64,
}

/// What the admin page shows.
pub struct Usage {
    pub files: usize,
    pub bytes: u64,
    pub max_bytes: u64,
}

fn is_key(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

impl DiskCache {
    /// Open (creating if needed) the cache directory and index what's in it,
    /// oldest-modified first. Leftover partial files are removed.
    pub async fn open(cfg: &CacheConfig) -> Result<Self> {
        let max_bytes = cfg.max_size_mb.saturating_mul(1024 * 1024);
        let max_file_bytes = cfg.max_file_mb.saturating_mul(1024 * 1024).min(max_bytes);
        Self::open_with(&cfg.dir, max_bytes, max_file_bytes).await
    }

    async fn open_with(dir: &Path, max_bytes: u64, max_file_bytes: u64) -> Result<Self> {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("create cache dir {}", dir.display()))?;
        let mut found = Vec::new();
        let mut rd = tokio::fs::read_dir(dir)
            .await
            .with_context(|| format!("read cache dir {}", dir.display()))?;
        while let Some(ent) = rd.next_entry().await? {
            let name = ent.file_name().to_string_lossy().into_owned();
            let meta = match ent.metadata().await {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            if is_key(&name) {
                let mtime = meta.modified().ok();
                found.push((name, meta.len(), mtime));
            } else if name.starts_with(SPOOL_PREFIX) || name.starts_with(FILL_PREFIX) {
                let _ = tokio::fs::remove_file(ent.path()).await;
            }
        }
        found.sort_by_key(|(_, _, mtime)| *mtime);

        let mut state = State::default();
        for (key, size, _) in found {
            state.clock += 1;
            state.total += size;
            state.entries.insert(
                key,
                Entry {
                    size,
                    last_used: state.clock,
                },
            );
        }
        let cache = Self {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                max_bytes,
                max_file_bytes,
                state: Mutex::new(state),
            }),
        };
        cache.evict_to_fit(0).await;
        Ok(cache)
    }

    fn path_of(&self, key: &str) -> PathBuf {
        self.inner.dir.join(key)
    }

    /// A fresh path in the cache directory to spool an upload into, so that
    /// keeping it afterwards is a rename on the same filesystem.
    pub fn spool_path(&self) -> PathBuf {
        self.inner
            .dir
            .join(format!("{SPOOL_PREFIX}{}", shortcode::generate(12)))
    }

    /// The cached file for `key`, opened, if there is one.
    pub async fn get(&self, key: &str) -> Option<File> {
        {
            let mut st = self.inner.state.lock().unwrap();
            st.clock += 1;
            let now = st.clock;
            st.entries.get_mut(key)?.last_used = now;
        }
        match File::open(self.path_of(key)).await {
            Ok(f) => Some(f),
            Err(_) => {
                // Gone from disk behind our back: forget it, and fetch from R2.
                self.forget(key);
                None
            }
        }
    }

    fn forget(&self, key: &str) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(e) = st.entries.remove(key) {
            st.total -= e.size;
        }
    }

    /// Keep a just-uploaded file whose bytes hash to `key`, moving it in from
    /// `tmp`. Either way `tmp` is gone afterwards.
    pub async fn adopt(&self, key: &str, tmp: &Path) {
        let size = match tokio::fs::metadata(tmp).await {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        let already = {
            let mut st = self.inner.state.lock().unwrap();
            st.clock += 1;
            let now = st.clock;
            match st.entries.get_mut(key) {
                Some(e) => {
                    e.last_used = now;
                    true
                }
                None => false,
            }
        };
        if already || size > self.inner.max_file_bytes {
            let _ = tokio::fs::remove_file(tmp).await;
            return;
        }
        self.evict_to_fit(size).await;
        if let Err(e) = tokio::fs::rename(tmp, self.path_of(key)).await {
            tracing::warn!(key, ?e, "could not move upload into cache");
            let _ = tokio::fs::remove_file(tmp).await;
            return;
        }
        self.record(key, size);
    }

    fn record(&self, key: &str, size: u64) {
        let mut st = self.inner.state.lock().unwrap();
        st.clock += 1;
        let now = st.clock;
        if let Some(old) = st.entries.insert(
            key.to_string(),
            Entry {
                size,
                last_used: now,
            },
        ) {
            st.total -= old.size;
        }
        st.total += size;
    }

    /// Drop least recently used files until `incoming` more bytes fit.
    async fn evict_to_fit(&self, incoming: u64) {
        let victims: Vec<String> = {
            let mut st = self.inner.state.lock().unwrap();
            let mut by_age: Vec<(u64, String)> = st
                .entries
                .iter()
                .map(|(k, e)| (e.last_used, k.clone()))
                .collect();
            by_age.sort();
            let mut victims = Vec::new();
            for (_, key) in by_age {
                if st.total + incoming <= self.inner.max_bytes {
                    break;
                }
                if let Some(e) = st.entries.remove(&key) {
                    st.total -= e.size;
                    victims.push(key);
                }
            }
            victims
        };
        for key in victims {
            // A request already streaming this file keeps its open handle.
            let _ = tokio::fs::remove_file(self.path_of(&key)).await;
        }
    }

    /// Remove `key`, e.g. after its object is deleted from R2.
    pub async fn remove(&self, key: &str) {
        self.forget(key);
        let _ = tokio::fs::remove_file(self.path_of(key)).await;
    }

    /// Remove everything. Returns how many files and bytes were dropped.
    pub async fn clear(&self) -> (usize, u64) {
        let (keys, bytes) = {
            let mut st = self.inner.state.lock().unwrap();
            let keys: Vec<String> = st.entries.drain().map(|(k, _)| k).collect();
            let bytes = std::mem::take(&mut st.total);
            (keys, bytes)
        };
        for key in &keys {
            let _ = tokio::fs::remove_file(self.path_of(key)).await;
        }
        (keys.len(), bytes)
    }

    pub fn usage(&self) -> Usage {
        let st = self.inner.state.lock().unwrap();
        Usage {
            files: st.entries.len(),
            bytes: st.total,
            max_bytes: self.inner.max_bytes,
        }
    }

    /// Start keeping a copy of `key` as it streams from R2. `None` when it is
    /// too large, already cached, or already being filled by another request.
    pub async fn start_fill(&self, key: &str, size: u64) -> Option<Fill> {
        if size > self.inner.max_file_bytes || !is_key(key) {
            return None;
        }
        {
            let mut st = self.inner.state.lock().unwrap();
            if st.entries.contains_key(key) || !st.filling.insert(key.to_string()) {
                return None;
            }
        }
        let tmp = self
            .inner
            .dir
            .join(format!("{FILL_PREFIX}{}", shortcode::generate(12)));
        match File::create(&tmp).await {
            Ok(file) => Some(Fill {
                cache: self.clone(),
                key: key.to_string(),
                tmp,
                file: Some(file),
                hasher: Sha256::new(),
                written: 0,
                expected: size,
                done: false,
            }),
            Err(e) => {
                tracing::warn!(key, ?e, "could not start cache fill");
                self.inner.state.lock().unwrap().filling.remove(key);
                None
            }
        }
    }
}

/// A copy of an R2 object being written as it streams to a client. Kept only
/// if `finish` sees every byte and they hash to the key; dropped otherwise
/// (say, when the client goes away halfway).
pub struct Fill {
    cache: DiskCache,
    key: String,
    tmp: PathBuf,
    file: Option<File>,
    hasher: Sha256,
    written: u64,
    expected: u64,
    done: bool,
}

impl Fill {
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        let file = self.file.as_mut().context("fill already closed")?;
        file.write_all(chunk).await?;
        self.hasher.update(chunk);
        self.written += chunk.len() as u64;
        Ok(())
    }

    /// Every expected byte has been written.
    pub fn complete(&self) -> bool {
        self.written >= self.expected
    }

    pub async fn finish(mut self) {
        let Some(mut file) = self.file.take() else {
            return;
        };
        if file.flush().await.is_err() {
            return;
        }
        drop(file);
        let hash = hex::encode(std::mem::take(&mut self.hasher).finalize());
        if self.written != self.expected || hash != self.key {
            tracing::warn!(key = %self.key, written = self.written, "cache fill did not match; not kept");
            return;
        }
        self.cache.evict_to_fit(self.written).await;
        if tokio::fs::rename(&self.tmp, self.cache.path_of(&self.key))
            .await
            .is_ok()
        {
            self.cache.record(&self.key, self.written);
            self.done = true;
        }
    }
}

impl Drop for Fill {
    fn drop(&mut self) {
        self.cache
            .inner
            .state
            .lock()
            .unwrap()
            .filling
            .remove(&self.key);
        if !self.done {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn scratch_dir() -> PathBuf {
        std::env::temp_dir().join(format!("zigi-cache-test-{}", shortcode::generate(10)))
    }

    async fn put(cache: &DiskCache, bytes: &[u8]) -> String {
        let key = key_of(bytes);
        let tmp = cache.spool_path();
        tokio::fs::write(&tmp, bytes).await.unwrap();
        cache.adopt(&key, &tmp).await;
        assert!(!tmp.exists(), "spool file should be moved or removed");
        key
    }

    async fn read(cache: &DiskCache, key: &str) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        let mut f = cache.get(key).await?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).await.unwrap();
        Some(v)
    }

    #[tokio::test]
    async fn keeps_uploads_and_drops_least_recently_used() {
        let dir = scratch_dir();
        let cache = DiskCache::open_with(&dir, 30, 30).await.unwrap();
        let a = put(&cache, &[b'a'; 10]).await;
        let b = put(&cache, &[b'b'; 10]).await;
        let c = put(&cache, &[b'c'; 10]).await;
        // Touch `a`, so `b` is now the least recently used.
        assert_eq!(read(&cache, &a).await.unwrap(), vec![b'a'; 10]);
        let d = put(&cache, &[b'd'; 10]).await;
        assert!(read(&cache, &b).await.is_none(), "b should be evicted");
        for k in [&a, &c, &d] {
            assert!(read(&cache, k).await.is_some());
        }
        assert_eq!(cache.usage().bytes, 30);
        assert!(!dir.join(&b).exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn skips_files_over_the_per_file_limit() {
        let dir = scratch_dir();
        let cache = DiskCache::open_with(&dir, 100, 5).await.unwrap();
        let k = put(&cache, &[b'x'; 6]).await;
        assert!(read(&cache, &k).await.is_none());
        assert_eq!(cache.usage().files, 0);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn reopening_finds_files_and_clears_leftovers() {
        let dir = scratch_dir();
        let cache = DiskCache::open_with(&dir, 100, 100).await.unwrap();
        let k = put(&cache, b"hello").await;
        tokio::fs::write(dir.join("upload-leftover"), b"x")
            .await
            .unwrap();
        tokio::fs::write(dir.join("fill-leftover"), b"x")
            .await
            .unwrap();
        drop(cache);

        let cache = DiskCache::open_with(&dir, 100, 100).await.unwrap();
        assert_eq!(read(&cache, &k).await.unwrap(), b"hello");
        assert!(!dir.join("upload-leftover").exists());
        assert!(!dir.join("fill-leftover").exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn fill_keeps_only_complete_matching_copies() {
        let dir = scratch_dir();
        let cache = DiskCache::open_with(&dir, 100, 100).await.unwrap();
        let body = b"streamed from r2";
        let key = key_of(body);

        // Abandoned halfway (client went away): nothing kept, no stray file.
        let mut fill = cache.start_fill(&key, body.len() as u64).await.unwrap();
        assert!(
            cache.start_fill(&key, body.len() as u64).await.is_none(),
            "one fill per key at a time"
        );
        fill.write(&body[..5]).await.unwrap();
        drop(fill);
        assert!(read(&cache, &key).await.is_none());

        // Wrong bytes for the key: not kept.
        let mut fill = cache.start_fill(&key, body.len() as u64).await.unwrap();
        fill.write(b"streamed from xx").await.unwrap();
        fill.finish().await;
        assert!(read(&cache, &key).await.is_none());

        // Whole and correct: kept.
        let mut fill = cache.start_fill(&key, body.len() as u64).await.unwrap();
        fill.write(&body[..7]).await.unwrap();
        fill.write(&body[7..]).await.unwrap();
        fill.finish().await;
        assert_eq!(read(&cache, &key).await.unwrap(), body);
        assert!(cache.start_fill(&key, body.len() as u64).await.is_none());

        let mut names = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec![key.clone()], "only the cached file is left");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn remove_and_clear() {
        let dir = scratch_dir();
        let cache = DiskCache::open_with(&dir, 100, 100).await.unwrap();
        let a = put(&cache, b"one").await;
        let b = put(&cache, b"two!").await;
        cache.remove(&a).await;
        assert!(read(&cache, &a).await.is_none());
        assert!(!dir.join(&a).exists());
        assert_eq!(cache.clear().await, (1, 4));
        assert!(read(&cache, &b).await.is_none());
        assert_eq!(cache.usage().bytes, 0);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
