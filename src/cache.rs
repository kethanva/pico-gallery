/// Simple disk-based image cache.
///
/// Images are stored as `<cache_dir>/<sanitised_key>-<fnv1a_hash>.jpg`.
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
/// Persist the LRU index every N puts. Batching avoids a full-JSON fs::write
/// on every cached photo — painful on Pi Zero's slow SD card. On crash, at
/// most (N-1) recent entries may become orphaned files inside the cache dir;
/// they'll be picked up by the next index-rewrite cycle.
const PUTS_PER_INDEX_SAVE: u32 = 8;

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
    puts_since_save: u32,
}

impl ImageCache {
    /// Create / open cache at `dir` with a `max_mb` ceiling.
    pub async fn open(dir: &Path, max_mb: u64) -> Result<Self> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating cache dir {}", dir.display()))?;

        // Restrict cache directory to owner-only: cached images are private photo data.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
                warn!(
                    "Could not restrict cache dir {} to 0700: {} — cached photos may be world-readable",
                    dir.display(), e
                );
            }
        }

        let max_bytes = max_mb * 1024 * 1024;
        let mut cache = Self {
            dir: dir.to_path_buf(),
            max_bytes,
            used_bytes: 0,
            lru: VecDeque::new(),
            puts_since_save: 0,
        };

        cache.load_index().await;
        info!(
            "Cache opened: {} MB used / {} MB limit",
            cache.used_bytes / 1_048_576,
            max_mb
        );
        Ok(cache)
    }

    // ── Public API ──────────────────────────────────────────────────────────

    /// Returns cached bytes if available.
    pub async fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let idx = self.lru.iter().position(|e| e.key == key)?;
        let entry = self.lru.remove(idx).unwrap();
        match fs::read(&entry.path).await {
            Ok(bytes) => {
                self.lru.push_back(entry); // refresh LRU position
                debug!("Cache HIT: {}", key);
                Some(bytes)
            }
            Err(e) => {
                warn!("Cache entry unreadable ({}): {}", key, e);
                self.used_bytes = self.used_bytes.saturating_sub(entry.size_bytes);
                // Persist the removal — otherwise a restart reloads the stale
                // entry from index.json and trips over it again.
                self.save_index().await;
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

        // An entry that cannot fit the configured budget is never stored.
        // Without this, the eviction loop below would empty the whole cache
        // trying to make room, then write anyway — leaving usage above max_mb.
        // It also makes max_mb = 0 a clean "cache disabled" (nothing ever fits).
        if size > self.max_bytes {
            debug!(
                "Cache skip (entry {} KB > budget {} KB): {}",
                size / 1024,
                self.max_bytes / 1024,
                key
            );
            return Ok(());
        }

        // Replace any existing entry for this key — a duplicate would
        // double-count used_bytes and let eviction of the old entry delete
        // the file the new entry still points at.
        if let Some(idx) = self.lru.iter().position(|e| e.key == key) {
            let old = self.lru.remove(idx).unwrap();
            self.used_bytes = self.used_bytes.saturating_sub(old.size_bytes);
        }

        // Evict until there's room. If eviction succeeds but the write below
        // fails, the evicted entries are gone for good (re-downloaded on next
        // showing) — acceptable on this single-user device; not worth the
        // complexity of a two-phase evict.
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

        // Batch index writes — see PUTS_PER_INDEX_SAVE doc comment.
        self.puts_since_save += 1;
        if self.puts_since_save >= PUTS_PER_INDEX_SAVE {
            self.save_index().await;
            self.puts_since_save = 0;
        }
        Ok(())
    }

    /// Force a sync of the LRU index to disk. Call before clean shutdown
    /// so any batched-but-unsaved entries become durable.
    pub async fn flush(&mut self) {
        if self.puts_since_save > 0 {
            self.save_index().await;
            self.puts_since_save = 0;
        }
    }

    /// True if the key is present (without promoting in LRU).
    pub fn contains(&self, key: &str) -> bool {
        self.lru.iter().any(|e| e.key == key)
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    fn path_for(&self, key: &str) -> PathBuf {
        // key is "plugin-name/photo-id" — sanitise for filesystem. The
        // separator→`_` replacement is lossy (`local/foo/bar` and
        // `local/foo_bar` collapse to the same name), so a stable FNV-1a
        // hash of the ORIGINAL key is appended to keep the mapping
        // injective. Truncation keeps long photo ids under filesystem
        // name limits; the hash preserves uniqueness regardless.
        // NOTE: adding the hash suffix changed cache filenames — existing
        // caches re-download once and old files age out via the index scan.
        const MAX_STEM_CHARS: usize = 100;
        let safe: String = key
            .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
            .chars()
            .take(MAX_STEM_CHARS)
            .collect();
        self.dir
            .join(format!("{}-{:016x}.jpg", safe, fnv1a_64(key)))
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
            Ok(data) => {
                let _ = fs::write(&index_path, data).await;
            }
            Err(e) => warn!("Failed to save cache index: {}", e),
        }
    }

    async fn load_index(&mut self) {
        let index_path = self.dir.join(INDEX_FILE);
        let data = match fs::read(&index_path).await {
            Ok(d) => d,
            Err(_) => return, // first run
        };
        let entries: VecDeque<CacheEntry> = match serde_json::from_slice(&data) {
            Ok(e) => e,
            Err(e) => {
                warn!("Cache index corrupt, rebuilding: {}", e);
                return;
            }
        };

        // Validate each entry still exists on disk and re-measure its size —
        // the persisted size_bytes is untrusted (files may have been
        // truncated or swapped behind our back), so used_bytes accounting
        // comes from fresh metadata, never from the JSON. N stat syscalls —
        // run on a blocking thread so the current_thread executor isn't
        // stalled at startup.
        let result = tokio::task::spawn_blocking(move || {
            let mut used = 0u64;
            let valid: VecDeque<_> = entries
                .into_iter()
                .filter_map(|mut e| {
                    let len = std::fs::metadata(&e.path).map(|m| m.len()).ok()?;
                    e.size_bytes = len;
                    used += len;
                    Some(e)
                })
                .collect();
            (valid, used)
        })
        .await;

        match result {
            Ok((valid, used)) => {
                self.used_bytes = used;
                self.lru = valid;
            }
            Err(e) => warn!("Cache index validation task failed: {}", e),
        }
    }
}

/// Stable 64-bit FNV-1a hash. Hand-rolled (~6 lines) because std's
/// `DefaultHasher` is not guaranteed stable across Rust releases and cache
/// filenames must survive upgrades; not worth a dependency.
fn fnv1a_64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for &byte in s.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    hash
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir under the system temp dir; removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            TempDir(std::env::temp_dir().join(format!(
                "picogallery-cache-test-{}-{tag}-{nanos}",
                std::process::id()
            )))
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn entry_larger_than_budget_is_not_cached() {
        let tmp = TempDir::new("oversize");
        let mut cache = ImageCache::open(&tmp.0, 1).await.unwrap(); // 1 MB budget
                                                                    // 2 MB blob: over the 1 MB budget but under MAX_ENTRY_BYTES.
        let big = vec![0u8; 2 * 1024 * 1024];
        cache.put("k/big", &big).await.unwrap();
        assert!(
            !cache.contains("k/big"),
            "over-budget entry must not be stored"
        );
        assert_eq!(cache.used_bytes, 0);
    }

    #[tokio::test]
    async fn entry_within_budget_is_cached() {
        let tmp = TempDir::new("fits");
        let mut cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        let small = vec![0u8; 256 * 1024]; // 256 KB
        cache.put("k/small", &small).await.unwrap();
        assert!(cache.contains("k/small"));
        assert_eq!(cache.used_bytes, small.len() as u64);
    }

    #[tokio::test]
    async fn zero_budget_disables_caching() {
        let tmp = TempDir::new("zero");
        let mut cache = ImageCache::open(&tmp.0, 0).await.unwrap();
        cache.put("k/x", &[1u8; 1024]).await.unwrap();
        assert!(!cache.contains("k/x"));
        assert_eq!(cache.used_bytes, 0);
    }
}
