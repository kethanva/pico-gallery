/// Simple disk-based image cache.
///
/// Images are stored as `<cache_dir>/<sanitised_key>-<fnv1a_hash>.jpg`.
/// An LRU index is maintained in memory and serialised to `<cache_dir>/index.json`.
/// On startup we scan the directory so the index survives restarts.
///
/// The LRU is an intrusive doubly-linked list over a slab (`nodes`) with a
/// `HashMap<key, slab-index>` for lookup, so `get`, `put`, and `contains` are
/// all O(1) — no per-access linear scan of the queue. `head` is the
/// least-recently-used end (evicted first); `tail` is most-recently-used.
use anyhow::{Context, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

const INDEX_FILE: &str = "index.json";
const MAX_ENTRY_BYTES: u64 = 20 * 1024 * 1024; // never cache a single item > 20 MB
/// Persist the LRU index every N puts. Batching avoids a full-JSON fs::write
/// on every cached photo — painful on Pi Zero's slow SD card. On crash, at
/// most (N-1) recent entries may become orphaned files inside the cache dir;
/// they'll be picked up by the next index-rewrite cycle.
const PUTS_PER_INDEX_SAVE: u32 = 8;

/// Persisted per-entry record. The on-disk `index.json` is a JSON array of
/// these in LRU order (head → tail), unchanged from earlier versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    path: PathBuf,
    size_bytes: u64,
}

/// Intrusive LRU node held in the `nodes` slab. Slab indices are stable while
/// a node lives, so `map` can point at them; freed slots are recycled via
/// `free`.
struct Node {
    entry: CacheEntry,
    prev: Option<usize>,
    next: Option<usize>,
}

pub struct ImageCache {
    dir: PathBuf,
    max_bytes: u64,
    used_bytes: u64,
    nodes: Vec<Node>,
    free: Vec<usize>,
    map: HashMap<String, usize>,
    head: Option<usize>, // least-recently-used — evicted first
    tail: Option<usize>, // most-recently-used
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
            nodes: Vec::new(),
            free: Vec::new(),
            map: HashMap::new(),
            head: None,
            tail: None,
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
        let idx = *self.map.get(key)?;
        let path = self.nodes[idx].entry.path.clone();
        match fs::read(&path).await {
            Ok(bytes) => {
                // Promote to most-recently-used.
                self.unlink(idx);
                self.push_tail(idx);
                debug!("Cache HIT: {}", key);
                Some(bytes)
            }
            Err(e) => {
                warn!("Cache entry unreadable ({}): {}", key, e);
                let size = self.nodes[idx].entry.size_bytes;
                self.remove_node(idx);
                self.used_bytes = self.used_bytes.saturating_sub(size);
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
        // the file the new entry still points at. Same path_for(key), so the
        // fs::write below overwrites the same file.
        if let Some(&idx) = self.map.get(key) {
            let old = self.nodes[idx].entry.size_bytes;
            self.remove_node(idx);
            self.used_bytes = self.used_bytes.saturating_sub(old);
        }

        // Evict until there's room. If eviction succeeds but the write below
        // fails, the evicted entries are gone for good (re-downloaded on next
        // showing) — acceptable on this single-user device; not worth the
        // complexity of a two-phase evict.
        while self.used_bytes + size > self.max_bytes && self.head.is_some() {
            self.evict_oldest().await;
        }

        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, bytes)
            .await
            .with_context(|| format!("writing cache entry {}", path.display()))?;

        let idx = self.alloc_node(CacheEntry {
            key: key.to_owned(),
            path,
            size_bytes: size,
        });
        self.map.insert(key.to_owned(), idx);
        self.push_tail(idx);
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
        self.map.contains_key(key)
    }

    // ── Intrusive LRU list helpers (all O(1)) ────────────────────────────────

    /// Take a slot from the free list (or grow the slab) and store `entry`.
    fn alloc_node(&mut self, entry: CacheEntry) -> usize {
        let node = Node {
            entry,
            prev: None,
            next: None,
        };
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = node;
            idx
        } else {
            self.nodes.push(node);
            self.nodes.len() - 1
        }
    }

    /// Detach `idx` from the linked list (leaves `map`/`free` untouched).
    fn unlink(&mut self, idx: usize) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        match prev {
            Some(p) => self.nodes[p].next = next,
            None => self.head = next,
        }
        match next {
            Some(n) => self.nodes[n].prev = prev,
            None => self.tail = prev,
        }
        self.nodes[idx].prev = None;
        self.nodes[idx].next = None;
    }

    /// Append `idx` at the tail (most-recently-used end).
    fn push_tail(&mut self, idx: usize) {
        self.nodes[idx].prev = self.tail;
        self.nodes[idx].next = None;
        match self.tail {
            Some(t) => self.nodes[t].next = Some(idx),
            None => self.head = Some(idx),
        }
        self.tail = Some(idx);
    }

    /// Fully remove `idx`: unlink, drop from `map`, recycle the slot.
    fn remove_node(&mut self, idx: usize) {
        self.unlink(idx);
        let key = std::mem::take(&mut self.nodes[idx].entry.key);
        self.map.remove(&key);
        self.free.push(idx);
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
        if let Some(idx) = self.head {
            let path = self.nodes[idx].entry.path.clone();
            let size = self.nodes[idx].entry.size_bytes;
            debug!("Cache evict: {}", self.nodes[idx].entry.key);
            self.remove_node(idx);
            let _ = fs::remove_file(&path).await;
            self.used_bytes = self.used_bytes.saturating_sub(size);
        }
    }

    async fn save_index(&self) {
        // Walk head → tail so the persisted order is LRU-first, matching the
        // historical VecDeque layout (a plain JSON array of CacheEntry).
        let mut ordered: Vec<&CacheEntry> = Vec::with_capacity(self.map.len());
        let mut cur = self.head;
        while let Some(idx) = cur {
            ordered.push(&self.nodes[idx].entry);
            cur = self.nodes[idx].next;
        }
        let index_path = self.dir.join(INDEX_FILE);
        match serde_json::to_vec(&ordered) {
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
        let entries: Vec<CacheEntry> = match serde_json::from_slice(&data) {
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
            let valid: Vec<CacheEntry> = entries
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
                // Rebuild the list in persisted order (head = LRU end).
                for entry in valid {
                    let key = entry.key.clone();
                    let idx = self.alloc_node(entry);
                    self.map.insert(key, idx);
                    self.push_tail(idx);
                }
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

    #[tokio::test]
    async fn get_returns_stored_bytes() {
        let tmp = TempDir::new("roundtrip");
        let mut cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        let data = vec![7u8; 1024];
        cache.put("k/a", &data).await.unwrap();
        assert_eq!(cache.get("k/a").await, Some(data));
        assert_eq!(cache.get("k/missing").await, None);
    }

    #[tokio::test]
    async fn evicts_least_recently_used_first() {
        let tmp = TempDir::new("lru-evict");
        let mut cache = ImageCache::open(&tmp.0, 1).await.unwrap(); // 1 MB
        let blob = vec![0u8; 400 * 1024]; // 400 KB each; three won't fit
        cache.put("k/a", &blob).await.unwrap();
        cache.put("k/b", &blob).await.unwrap();
        cache.put("k/c", &blob).await.unwrap(); // evicts oldest (a)
        assert!(!cache.contains("k/a"), "oldest entry must be evicted");
        assert!(cache.contains("k/b"));
        assert!(cache.contains("k/c"));
        assert!(cache.used_bytes <= cache.max_bytes);
    }

    #[tokio::test]
    async fn get_promotes_recency_and_protects_from_eviction() {
        let tmp = TempDir::new("lru-promote");
        let mut cache = ImageCache::open(&tmp.0, 1).await.unwrap();
        let blob = vec![0u8; 400 * 1024];
        cache.put("k/a", &blob).await.unwrap();
        cache.put("k/b", &blob).await.unwrap();
        // Touch a → now b is the least-recently-used.
        assert!(cache.get("k/a").await.is_some());
        cache.put("k/c", &blob).await.unwrap(); // should evict b, not a
        assert!(cache.contains("k/a"), "recently-read entry must survive");
        assert!(!cache.contains("k/b"), "untouched entry evicted first");
        assert!(cache.contains("k/c"));
    }

    #[tokio::test]
    async fn replacing_key_updates_size_without_duplicating() {
        let tmp = TempDir::new("replace");
        let mut cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        cache.put("k/x", &vec![0u8; 256 * 1024]).await.unwrap();
        cache.put("k/x", &vec![0u8; 512 * 1024]).await.unwrap();
        assert!(cache.contains("k/x"));
        assert_eq!(cache.used_bytes, 512 * 1024, "size must reflect newest put");
        assert_eq!(cache.map.len(), 1, "no duplicate key in the index");
    }

    #[tokio::test]
    async fn index_survives_reopen() {
        let tmp = TempDir::new("persist");
        {
            let mut cache = ImageCache::open(&tmp.0, 4).await.unwrap();
            cache.put("k/a", &vec![1u8; 256 * 1024]).await.unwrap();
            cache.put("k/b", &vec![2u8; 256 * 1024]).await.unwrap();
            cache.flush().await;
        }
        let mut reopened = ImageCache::open(&tmp.0, 4).await.unwrap();
        assert!(reopened.contains("k/a"));
        assert!(reopened.contains("k/b"));
        assert_eq!(reopened.used_bytes, 512 * 1024);
        // Freed-slot reuse path still resolves correctly after a reload.
        assert_eq!(reopened.get("k/a").await, Some(vec![1u8; 256 * 1024]));
    }
}
