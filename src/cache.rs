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
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

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

        // Restrict cache directory to owner-only if it looks like our default dir.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if dir.ends_with("picogallery") {
                if let Err(e) =
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                {
                    warn!(
                        "Could not restrict cache dir {} to 0700: {} — cached photos may be world-readable",
                        dir.display(), e
                    );
                }
            }
        }

        let max_bytes = max_mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow::anyhow!("cache size overflows bytes"))?;
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
        let mut stale_paths = Vec::new();
        while cache.used_bytes > cache.max_bytes && cache.head.is_some() {
            if let Some(path) = cache.take_oldest() {
                stale_paths.push(path);
            }
        }
        remove_cache_files(stale_paths).await;
        cache.save_index().await;
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
        let mut stale_paths = Vec::new();
        while self.used_bytes + size > self.max_bytes && self.head.is_some() {
            if let Some(path) = self.take_oldest() {
                stale_paths.push(path);
            }
        }
        // Delete a whole eviction run inside one blocking task. Reducing a
        // cache budget can remove hundreds of files; dispatching one Tokio
        // blocking job per unlink made startup cleanup needlessly slow.
        remove_cache_files(stale_paths).await;

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

    /// Remove one entry immediately. Used when a cached file can be read but
    /// fails image decoding, so a damaged download does not poison every
    /// future slideshow cycle. Returns whether an indexed entry existed.
    pub async fn remove(&mut self, key: &str) -> bool {
        let Some(&idx) = self.map.get(key) else {
            return false;
        };
        let path = self.nodes[idx].entry.path.clone();
        let size = self.nodes[idx].entry.size_bytes;
        self.remove_node(idx);
        self.used_bytes = self.used_bytes.saturating_sub(size);
        if let Err(error) = fs::remove_file(&path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!("Could not remove cache entry {}: {}", path.display(), error);
            }
        }
        // Bad entries must not return after restart even when the normal index
        // writer is currently inside its eight-put batching window.
        self.save_index().await;
        true
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

    fn take_oldest(&mut self) -> Option<PathBuf> {
        let idx = self.head?;
        let path = self.nodes[idx].entry.path.clone();
        let size = self.nodes[idx].entry.size_bytes;
        debug!("Cache evict: {}", self.nodes[idx].entry.key);
        self.remove_node(idx);
        self.used_bytes = self.used_bytes.saturating_sub(size);
        Some(path)
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
        match serde_json::to_vec(&ordered) {
            Ok(data) => {
                let index_path = self.dir.join(INDEX_FILE);
                if let Err(e) = atomic_write(&index_path, &data).await {
                    warn!("Failed to atomically save cache index: {e}");
                }
            }
            Err(e) => warn!("Failed to save cache index: {}", e),
        }
    }

    async fn load_index(&mut self) {
        let index_path = self.dir.join(INDEX_FILE);
        let entries: Vec<CacheEntry> = match fs::read(&index_path).await {
            Ok(data) => match serde_json::from_slice(&data) {
                Ok(entries) => entries,
                Err(e) => {
                    warn!("Cache index corrupt, rebuilding: {}", e);
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                warn!("Could not read cache index: {e}");
                Vec::new()
            }
        };

        // Validate each entry still exists on disk and re-measure its size —
        // the persisted size_bytes is untrusted (files may have been
        // truncated or swapped behind our back), so used_bytes accounting
        // comes from fresh metadata, never from the JSON. N stat syscalls —
        // run on a blocking thread so the current_thread executor isn't
        // stalled at startup.
        let cache_root = match std::fs::canonicalize(&self.dir) {
            Ok(path) => path,
            Err(e) => {
                warn!("Cache root cannot be canonicalized: {e}");
                return;
            }
        };
        let result = tokio::task::spawn_blocking(move || {
            let mut used = 0u64;
            let valid: Vec<CacheEntry> = entries
                .into_iter()
                .filter_map(|mut e| {
                    let canonical = std::fs::canonicalize(&e.path).ok()?;
                    if !canonical.starts_with(&cache_root) {
                        return None;
                    }
                    let len = std::fs::metadata(&canonical).map(|m| m.len()).ok()?;
                    e.path = canonical;
                    e.size_bytes = len;
                    used += len;
                    Some(e)
                })
                .collect();
            let known: std::collections::HashSet<PathBuf> =
                valid.iter().map(|entry| entry.path.clone()).collect();
            if let Ok(read_dir) = std::fs::read_dir(&cache_root) {
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if let Some(stem) = fname.strip_suffix(".jpg") {
                        let bytes = stem.as_bytes();
                        if bytes.len() >= 17
                            && bytes[bytes.len() - 17] == b'-'
                            && bytes[bytes.len() - 16..].iter().all(u8::is_ascii_hexdigit)
                            && !known.contains(&path)
                        {
                            let _ = std::fs::remove_file(&path);
                        }
                    } else if fname.starts_with('.')
                        && fname.contains(INDEX_FILE)
                        && fname.ends_with(".tmp")
                    {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
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

async fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let path = path.to_path_buf();
    let data = data.to_vec();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "index path has no parent")
        })?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let tmp = parent.join(format!(".{}.{}.tmp", INDEX_FILE, nonce));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

async fn remove_cache_files(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let result = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter_map(|path| match std::fs::remove_file(&path) {
                Ok(()) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => Some((path, error)),
            })
            .collect::<Vec<_>>()
    })
    .await;
    match result {
        Ok(errors) => {
            for (path, error) in errors {
                warn!("Could not evict cache entry {}: {}", path.display(), error);
            }
        }
        Err(error) => warn!("Cache eviction task failed: {error}"),
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

/// The engine's handle to the disk LRU. `None` inside means the cache could not
/// be opened (unwritable dir, full SD card) and every operation is a no-op —
/// spec §5 requires degrading to no-cache operation, not failing startup.
#[derive(Clone)]
pub struct CacheHandle(Option<Arc<Mutex<ImageCache>>>);

impl CacheHandle {
    /// Open the cache, degrading to a disabled handle on any failure.
    /// Logs once at `warn` with the cause; never returns `Err`.
    pub async fn open_or_degrade(dir: &Path, max_mb: u64) -> Self {
        match ImageCache::open(dir, max_mb).await {
            Ok(cache) => Self(Some(Arc::new(Mutex::new(cache)))),
            Err(e) => {
                warn!(
                    "Cache disabled (could not open {}): {e:#} — photos will be fetched every time",
                    dir.display()
                );
                Self(None)
            }
        }
    }

    /// Explicitly disabled handle (tests, or when the operator set max_mb = 0
    /// and we still want a typed object rather than skipping construction).
    pub fn disabled() -> Self {
        Self(None)
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    pub async fn get(&self, key: &str) -> Option<Vec<u8>> {
        match &self.0 {
            Some(cache) => cache.lock().await.get(key).await,
            None => None,
        }
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) {
        if let Some(cache) = &self.0 {
            if let Err(e) = cache.lock().await.put(key, bytes).await {
                warn!("Cache put failed ({key}): {e}");
            }
        }
    }

    pub async fn remove(&self, key: &str) {
        if let Some(cache) = &self.0 {
            let _ = cache.lock().await.remove(key).await;
        }
    }

    pub async fn flush(&self) {
        if let Some(cache) = &self.0 {
            cache.lock().await.flush().await;
        }
    }
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

    #[tokio::test]
    async fn reopen_removes_orphaned_image_files() {
        let tmp = TempDir::new("orphans");
        std::fs::create_dir_all(&tmp.0).unwrap();
        let orphan = tmp.0.join("orphan-0123456789abcdef.jpg");
        std::fs::write(&orphan, b"orphan").unwrap();
        let cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        assert!(!orphan.exists());
        assert_eq!(cache.used_bytes, 0);
    }

    #[tokio::test]
    async fn reopen_does_not_panic_on_non_ascii_jpg_names() {
        let tmp = TempDir::new("utf8_orphan");
        std::fs::create_dir_all(&tmp.0).unwrap();
        // Byte index len-21 lands inside `é` (2-byte UTF-8) for the old
        // `&fname[len-21..]` scan — must not panic.
        let junk = tmp.0.join("éaaaaaaaaaaaaaaaa.jpg");
        std::fs::write(&junk, b"junk").unwrap();
        let cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        assert!(junk.exists(), "non-engine jpg names must be left alone");
        assert_eq!(cache.used_bytes, 0);
    }

    #[tokio::test]
    async fn reopen_prunes_stale_index_tmp_files() {
        let tmp = TempDir::new("index_tmp");
        std::fs::create_dir_all(&tmp.0).unwrap();
        let stale = tmp.0.join(format!(".{INDEX_FILE}.1.tmp"));
        std::fs::write(&stale, b"{}").unwrap();
        let _cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn test_fnv1a_64() {
        // Known stable hashes for these inputs
        assert_eq!(fnv1a_64("hello"), 0xa430d84680aabd0b);
        assert_eq!(fnv1a_64("picogallery/photo1"), 4730821401576092488);
    }

    #[tokio::test]
    async fn atomic_write_creates_file_properly() {
        let tmp = TempDir::new("atomic_write");
        std::fs::create_dir_all(&tmp.0).unwrap();
        let file_path = tmp.0.join("test.json");

        atomic_write(&file_path, b"test_data").await.unwrap();
        let read = std::fs::read(&file_path).unwrap();
        assert_eq!(read, b"test_data");
    }

    #[tokio::test]
    async fn atomic_write_fails_gracefully_on_missing_parent() {
        let path = PathBuf::from("/this/path/does/not/exist/test.json");
        let result = atomic_write(&path, b"data").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn load_index_recovers_from_corrupt_json() {
        let tmp = TempDir::new("corrupt_json");
        std::fs::create_dir_all(&tmp.0).unwrap();
        let index_path = tmp.0.join(INDEX_FILE);
        std::fs::write(&index_path, b"{invalid_json}").unwrap();

        let cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        assert_eq!(cache.used_bytes, 0);
        assert_eq!(cache.map.len(), 0);
    }

    #[tokio::test]
    async fn get_removes_entry_when_file_deleted_externally() {
        let tmp = TempDir::new("external_delete");
        let mut cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        cache.put("k/a", &vec![1u8; 1024]).await.unwrap();

        // Find the actual path and delete it manually
        let path = cache.path_for("k/a");
        std::fs::remove_file(&path).unwrap();

        // This should fail to get and remove the entry
        let result = cache.get("k/a").await;
        assert!(result.is_none());
        assert!(!cache.contains("k/a"));
        assert_eq!(cache.used_bytes, 0);
    }

    #[tokio::test]
    async fn remove_drops_index_and_file() {
        let tmp = TempDir::new("remove");
        let mut cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        cache.put("bad/photo", b"corrupt-jpeg").await.unwrap();
        let path = cache.path_for("bad/photo");

        assert!(cache.remove("bad/photo").await);
        assert!(!cache.contains("bad/photo"));
        assert!(!path.exists());
        assert_eq!(cache.used_bytes, 0);
        assert!(!cache.remove("bad/photo").await);
    }

    #[tokio::test]
    async fn open_or_degrade_returns_disabled_handle_on_unwritable_dir() {
        let tmp = TempDir::new("degrade");
        std::fs::create_dir_all(&tmp.0).unwrap();
        // Parent is a regular file, so create_dir_all of a child must fail.
        let as_file = tmp.0.join("not-a-dir");
        std::fs::write(&as_file, b"x").unwrap();
        let handle = CacheHandle::open_or_degrade(&as_file.join("cache"), 4).await;
        assert!(!handle.is_enabled());
        assert!(handle.get("k").await.is_none());
        handle.put("k", b"bytes").await;
        handle.remove("k").await;
        handle.flush().await;
    }

    #[tokio::test]
    async fn load_index_handles_non_ascii_and_cleans_orphans() {
        let tmp = TempDir::new("non_ascii_orphans");
        std::fs::create_dir_all(&tmp.0).unwrap();

        // Write a file with multibyte UTF-8 characters and valid-looking suffix
        let utf8_file = tmp.0.join("café_küche_été-0123456789abcdef.jpg");
        std::fs::write(&utf8_file, b"dummy data").unwrap();

        // Write an orphan .index.json.nonce.tmp file
        let orphan_tmp = tmp.0.join(".index.json.12345.tmp");
        std::fs::write(&orphan_tmp, b"temp content").unwrap();

        // Opening the cache scans the directory, shouldn't panic on UTF-8, and should remove the unindexed file & orphan tmp
        let cache = ImageCache::open(&tmp.0, 4).await.unwrap();
        assert_eq!(cache.used_bytes, 0);
        assert!(!utf8_file.exists());
        assert!(!orphan_tmp.exists());
    }
}
