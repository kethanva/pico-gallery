/// Simple disk-based image cache.
///
/// Images are stored as `<cache_dir>/<plugin>/<photo_id>.jpg`.
/// An LRU index is maintained in memory and serialised to `<cache_dir>/index.json`.
/// On startup we scan the directory so the index survives restarts.
use anyhow::{Context, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use tokio::fs;

const INDEX_FILE: &str = "index.json";
const MAX_ENTRY_BYTES: u64 = 20 * 1024 * 1024; // never cache a single item > 20 MB

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    path: PathBuf,
    size_bytes: u64,
}

pub struct ImageCache {
    dir: PathBuf,
    max_bytes: u64,
    used_bytes: u64,
    lru: VecDeque<CacheEntry>,
}

impl ImageCache {
    /// Create / open cache at `dir` with a `max_mb` ceiling.
    pub async fn open(dir: &Path, max_mb: u64) -> Result<Self> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating cache dir {}", dir.display()))?;

        let max_bytes = max_mb * 1024 * 1024;
        let mut cache = Self {
            dir: dir.to_path_buf(),
            max_bytes,
            used_bytes: 0,
            lru: VecDeque::new(),
        };

        cache.load_index().await;
        info!("Cache opened: {} MB used / {} MB limit", cache.used_bytes / 1_048_576, max_mb);
        Ok(cache)
    }

    // ── Public API ──────────────────────────────────────────────────────────

    /// Returns cached bytes if available.
    pub async fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let idx = self.lru.iter().position(|e| e.key == key)?;
        let entry = self.lru.remove(idx).unwrap();
        match fs::read(&entry.path).await {
            Ok(bytes) => {
                self.lru.push_back(entry);   // refresh LRU position
                debug!("Cache HIT: {}", key);
                Some(bytes)
            }
            Err(e) => {
                warn!("Cache entry unreadable ({}): {}", key, e);
                self.used_bytes = self.used_bytes.saturating_sub(entry.size_bytes);
                None
            }
        }
    }

    /// Store `bytes` under `key`.  Evicts old entries if over budget.
    pub async fn put(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        let size = bytes.len() as u64;
        if size > MAX_ENTRY_BYTES {
            return Ok(()); // don't cache oversized blobs
        }

        // Evict until there's room.
        while self.used_bytes + size > self.max_bytes && !self.lru.is_empty() {
            self.evict_oldest().await;
        }

        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, bytes)
            .await
            .with_context(|| format!("writing cache entry {}", path.display()))?;

        self.lru.push_back(CacheEntry {
            key: key.to_owned(),
            path,
            size_bytes: size,
        });
        self.used_bytes += size;
        debug!("Cache PUT: {} ({} KB)", key, size / 1024);
        self.save_index().await;
        Ok(())
    }

    /// True if the key is present (without promoting in LRU).
    pub fn contains(&self, key: &str) -> bool {
        self.lru.iter().any(|e| e.key == key)
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    fn path_for(&self, key: &str) -> PathBuf {
        // key is "plugin-name/photo-id" — sanitise for filesystem
        let safe = key.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
        self.dir.join(format!("{}.jpg", safe))
    }

    async fn evict_oldest(&mut self) {
        if let Some(entry) = self.lru.pop_front() {
            debug!("Cache evict: {}", entry.key);
            let _ = fs::remove_file(&entry.path).await;
            self.used_bytes = self.used_bytes.saturating_sub(entry.size_bytes);
        }
    }

    async fn save_index(&self) {
        let index_path = self.dir.join(INDEX_FILE);
        match serde_json::to_vec(&self.lru) {
            Ok(data) => { let _ = fs::write(&index_path, data).await; }
            Err(e)   => warn!("Failed to save cache index: {}", e),
        }
    }

    async fn load_index(&mut self) {
        let index_path = self.dir.join(INDEX_FILE);
        let data = match fs::read(&index_path).await {
            Ok(d) => d,
            Err(_) => return,  // first run
        };
        let entries: VecDeque<CacheEntry> = match serde_json::from_slice(&data) {
            Ok(e) => e,
            Err(e) => { warn!("Cache index corrupt, rebuilding: {}", e); return; }
        };

        // Validate each entry still exists on disk.
        let mut used = 0u64;
        let valid: VecDeque<_> = entries.into_iter().filter(|e| {
            if e.path.exists() { used += e.size_bytes; true } else { false }
        }).collect();

        self.used_bytes = used;
        self.lru = valid;
    }
}
