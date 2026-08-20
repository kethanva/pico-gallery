//! Background queue extend + favourite toggle so the display loop never awaits
//! plugin `list_photos` / `set_favorite`.
//!
//! Same pattern as [`crate::fetcher::Fetcher`]: sync [`QueueIo::try_spawn_extend`] /
//! [`QueueIo::try_spawn_favorite`], sync [`QueueIo::drain_extend`] /
//! [`QueueIo::drain_favorite`]. Stale results are dropped via generation bump.

use log::{info, warn};
use picogallery_core::{PhotoMeta, PhotoPlugin};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Photos fetched per API page.
pub const PAGE_SIZE: usize = 50;
/// Hard cap per plugin when paging the remote library into the play queue.
pub const MAX_PHOTOS_PER_PLUGIN: usize = 2000;
/// Fetch the next API page when navigation is within this many items of the end.
pub const LOAD_AHEAD_MARGIN: usize = 30;
/// Max `list_photos` rounds per extend job (mirrors former `extend_queue_once`).
pub const EXTEND_MAX_ROUNDS: usize = 8;

const LIST_PHOTOS_TIMEOUT: Duration = Duration::from_secs(30);
const FAVORITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Tracks how many photos have been pulled from each plugin so far.
#[derive(Clone)]
pub struct QueueLoader {
    pub(crate) plugin_offsets: Vec<usize>,
    pub(crate) plugin_exhausted: Vec<bool>,
    pub(crate) plugin_retryable_error: Vec<bool>,
    pub(crate) retry_after: Vec<Option<Instant>>,
    pub(crate) consecutive_errors: Vec<u8>,
    pub shuffle_seed: u64,
}

impl QueueLoader {
    pub fn new(plugin_count: usize) -> Self {
        Self {
            plugin_offsets: vec![0; plugin_count],
            plugin_exhausted: vec![false; plugin_count],
            plugin_retryable_error: vec![false; plugin_count],
            retry_after: vec![None; plugin_count],
            consecutive_errors: vec![0; plugin_count],
            shuffle_seed: shuffle_seed_now(),
        }
    }

    pub fn all_exhausted(&self) -> bool {
        self.plugin_exhausted.iter().all(|&e| e)
    }

    pub fn mark_fully_loaded(&mut self) {
        self.plugin_exhausted.fill(true);
    }

    /// Recompute per-plugin item counts from an externally-built `queue` (e.g.
    /// the single-round initial load). A nonzero count that isn't an exact
    /// multiple of `PAGE_SIZE` means the last page was short and exhausted.
    /// Zero is deliberately retryable: it may represent a transient initial
    /// failure from a provider that has not contributed a page yet.
    pub fn sync_counts(&mut self, queue: &[(usize, PhotoMeta)]) {
        self.plugin_offsets.fill(0);
        for (pi, _) in queue {
            if *pi < self.plugin_offsets.len() {
                self.plugin_offsets[*pi] += 1;
            }
        }
        for (i, off) in self.plugin_offsets.iter().enumerate() {
            if *off >= MAX_PHOTOS_PER_PLUGIN || (*off > 0 && *off % PAGE_SIZE != 0) {
                self.plugin_exhausted[i] = true;
            }
        }
    }

    pub fn near_end(&self, trigger_idx: usize, queue_len: usize) -> bool {
        !self.all_exhausted()
            && queue_len > 0
            && trigger_idx + LOAD_AHEAD_MARGIN >= queue_len.saturating_sub(1)
    }

    pub fn ready_to_retry(&self, plugin_idx: usize) -> bool {
        self.retry_after[plugin_idx].is_none_or(|deadline| Instant::now() >= deadline)
    }

    pub fn record_success(&mut self, plugin_idx: usize) {
        self.plugin_retryable_error[plugin_idx] = false;
        self.consecutive_errors[plugin_idx] = 0;
        self.retry_after[plugin_idx] = None;
    }

    pub fn record_error(&mut self, plugin_idx: usize) -> Duration {
        self.plugin_retryable_error[plugin_idx] = true;
        self.consecutive_errors[plugin_idx] = self.consecutive_errors[plugin_idx].saturating_add(1);
        let exponent = u32::from(self.consecutive_errors[plugin_idx].saturating_sub(1)).min(6);
        let delay = Duration::from_secs(1u64 << exponent);
        self.retry_after[plugin_idx] = Some(Instant::now() + delay);
        delay
    }
}

fn shuffle_seed_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42)
}

/// Keep only `(plugin_idx, id)` pairs not already recorded in `seen`.
/// Inserts kept keys into `seen` so callers can chain multiple batches.
pub fn filter_unseen_photos(
    batch: Vec<(usize, PhotoMeta)>,
    seen: &mut HashSet<(usize, String)>,
) -> Vec<(usize, PhotoMeta)> {
    batch
        .into_iter()
        .filter(|(pi, m)| seen.insert((*pi, m.id.clone())))
        .collect()
}

/// Pull one API page from every plugin that still has more photos.
pub async fn fetch_queue_round(
    plugins: &[Arc<dyn PhotoPlugin>],
    loader: &mut QueueLoader,
    page_size: usize,
    max_per_plugin: usize,
) -> Vec<(usize, PhotoMeta)> {
    let mut batch = Vec::new();
    for (plugin_idx, plugin) in plugins.iter().enumerate() {
        if loader.plugin_exhausted[plugin_idx] {
            continue;
        }
        if !loader.ready_to_retry(plugin_idx) {
            continue;
        }
        let offset = loader.plugin_offsets[plugin_idx];
        if offset >= max_per_plugin {
            loader.plugin_exhausted[plugin_idx] = true;
            continue;
        }
        match tokio::time::timeout(LIST_PHOTOS_TIMEOUT, plugin.list_photos(page_size, offset)).await
        {
            Ok(Ok(page)) if page.is_empty() => {
                loader.record_success(plugin_idx);
                loader.plugin_exhausted[plugin_idx] = true;
            }
            Ok(Ok(page)) => {
                loader.record_success(plugin_idx);
                info!(
                    "  {} loaded {} photos (offset {})",
                    plugin.name(),
                    page.len(),
                    offset
                );
                let n = page.len();
                loader.plugin_offsets[plugin_idx] += n;
                batch.extend(page.into_iter().map(|m| (plugin_idx, m)));
                if n < page_size || loader.plugin_offsets[plugin_idx] >= max_per_plugin {
                    loader.plugin_exhausted[plugin_idx] = true;
                }
            }
            Ok(Err(e)) => {
                let delay = loader.record_error(plugin_idx);
                warn!(
                    "  {} list_photos error: {}; retrying in {}s",
                    plugin.name(),
                    e,
                    delay.as_secs()
                );
            }
            Err(_) => {
                let delay = loader.record_error(plugin_idx);
                warn!(
                    "  {} list_photos timed out after {}s; retrying in {}s",
                    plugin.name(),
                    LIST_PHOTOS_TIMEOUT.as_secs(),
                    delay.as_secs()
                );
            }
        }
    }
    batch
}

pub struct ExtendResult {
    pub generation: u64,
    pub loader: QueueLoader,
    pub photos: Vec<(usize, PhotoMeta)>,
}

pub struct FavoriteResult {
    pub generation: u64,
    pub plugin_idx: usize,
    pub photo_id: String,
    pub favorite: bool,
    pub ok: bool,
    pub error: Option<String>,
}

pub struct QueueIo {
    extend_tx: mpsc::Sender<ExtendResult>,
    extend_rx: mpsc::Receiver<ExtendResult>,
    fav_tx: mpsc::Sender<FavoriteResult>,
    fav_rx: mpsc::Receiver<FavoriteResult>,
    extend_in_flight: bool,
    fav_in_flight: bool,
    generation: u64,
}

impl QueueIo {
    pub fn new() -> Self {
        let (extend_tx, extend_rx) = mpsc::channel(2);
        let (fav_tx, fav_rx) = mpsc::channel(2);
        Self {
            extend_tx,
            extend_rx,
            fav_tx,
            fav_rx,
            extend_in_flight: false,
            fav_in_flight: false,
            generation: 0,
        }
    }

    /// Bump generation so in-flight results are dropped on drain.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn extend_in_flight(&self) -> bool {
        self.extend_in_flight
    }

    pub fn fav_in_flight(&self) -> bool {
        self.fav_in_flight
    }

    /// Sync. Spawns background `list_photos` paging. Returns false if already in flight.
    pub fn try_spawn_extend(
        &mut self,
        plugins: Vec<Arc<dyn PhotoPlugin>>,
        mut loader: QueueLoader,
        mut known_ids: HashSet<(usize, String)>,
        max_rounds: usize,
        page_size: usize,
        max_per_plugin: usize,
    ) -> bool {
        if self.extend_in_flight {
            return false;
        }
        if loader.all_exhausted() {
            return false;
        }
        let tx = self.extend_tx.clone();
        let generation = self.generation;
        self.extend_in_flight = true;
        tokio::spawn(async move {
            let mut unique: Vec<(usize, PhotoMeta)> = Vec::new();
            for _ in 0..max_rounds {
                if loader.all_exhausted() {
                    break;
                }
                let batch =
                    fetch_queue_round(&plugins, &mut loader, page_size, max_per_plugin).await;
                if batch.is_empty() {
                    break;
                }
                let fresh = filter_unseen_photos(batch, &mut known_ids);
                if fresh.is_empty() {
                    continue;
                }
                unique.extend(fresh);
                if unique.len() >= page_size {
                    break;
                }
            }
            let _ = tx
                .send(ExtendResult {
                    generation,
                    loader,
                    photos: unique,
                })
                .await;
        });
        true
    }

    /// Sync. Spawns `set_favorite` with a 30s timeout. Returns false if already in flight.
    pub fn try_spawn_favorite(
        &mut self,
        plugin: Arc<dyn PhotoPlugin>,
        plugin_idx: usize,
        meta: PhotoMeta,
        target: bool,
    ) -> bool {
        if self.fav_in_flight {
            return false;
        }
        let tx = self.fav_tx.clone();
        let generation = self.generation;
        let photo_id = meta.id.clone();
        self.fav_in_flight = true;
        tokio::spawn(async move {
            let result =
                match tokio::time::timeout(FAVORITE_TIMEOUT, plugin.set_favorite(&meta, target))
                    .await
                {
                    Ok(Ok(())) => FavoriteResult {
                        generation,
                        plugin_idx,
                        photo_id,
                        favorite: target,
                        ok: true,
                        error: None,
                    },
                    Ok(Err(e)) => FavoriteResult {
                        generation,
                        plugin_idx,
                        photo_id,
                        favorite: target,
                        ok: false,
                        error: Some(e.to_string()),
                    },
                    Err(_) => FavoriteResult {
                        generation,
                        plugin_idx,
                        photo_id,
                        favorite: target,
                        ok: false,
                        error: Some(format!(
                            "Favourite toggle timed out after {} s",
                            FAVORITE_TIMEOUT.as_secs()
                        )),
                    },
                };
            let _ = tx.send(result).await;
        });
        true
    }

    /// Sync. `try_recv`; clears in-flight; drops stale generation.
    pub fn drain_extend(&mut self) -> Option<ExtendResult> {
        match self.extend_rx.try_recv() {
            Ok(done) => {
                self.extend_in_flight = false;
                if done.generation != self.generation {
                    return None;
                }
                Some(done)
            }
            Err(_) => None,
        }
    }

    /// Sync. `try_recv`; clears in-flight; drops stale generation.
    pub fn drain_favorite(&mut self) -> Option<FavoriteResult> {
        match self.fav_rx.try_recv() {
            Ok(done) => {
                self.fav_in_flight = false;
                if done.generation != self.generation {
                    return None;
                }
                Some(done)
            }
            Err(_) => None,
        }
    }
}

impl Default for QueueIo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use picogallery_core::{AuthStatus, PluginConfig};
    use std::time::Instant;

    struct SlowListPlugin {
        delay: Duration,
    }

    #[async_trait]
    impl PhotoPlugin for SlowListPlugin {
        fn name(&self) -> &str {
            "slow-list"
        }
        async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
            Ok(())
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus::Authenticated
        }
        async fn authenticate(&mut self) -> Result<AuthStatus> {
            Ok(AuthStatus::Authenticated)
        }
        async fn list_photos(&self, _limit: usize, _offset: usize) -> Result<Vec<PhotoMeta>> {
            tokio::time::sleep(self.delay).await;
            Ok(vec![PhotoMeta {
                id: "p1".into(),
                filename: "p1.jpg".into(),
                width: 10,
                height: 10,
                taken_at: None,
                download_url: None,
                album: None,
                title: None,
                location: None,
                is_favorite: false,
                extra: Default::default(),
            }])
        }
        async fn get_photo_bytes(
            &self,
            _meta: &PhotoMeta,
            _intent: picogallery_core::FetchIntent,
        ) -> Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
    }

    struct SlowFavoritePlugin {
        delay: Duration,
    }

    #[async_trait]
    impl PhotoPlugin for SlowFavoritePlugin {
        fn name(&self) -> &str {
            "slow-fav"
        }
        async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
            Ok(())
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus::Authenticated
        }
        async fn authenticate(&mut self) -> Result<AuthStatus> {
            Ok(AuthStatus::Authenticated)
        }
        async fn list_photos(&self, _limit: usize, _offset: usize) -> Result<Vec<PhotoMeta>> {
            Ok(Vec::new())
        }
        async fn get_photo_bytes(
            &self,
            _meta: &PhotoMeta,
            _intent: picogallery_core::FetchIntent,
        ) -> Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
        async fn set_favorite(&self, _meta: &PhotoMeta, _favorite: bool) -> Result<()> {
            tokio::time::sleep(self.delay).await;
            Ok(())
        }
    }

    fn dummy_meta() -> PhotoMeta {
        PhotoMeta {
            id: "p1".into(),
            filename: "p1.jpg".into(),
            width: 10,
            height: 10,
            taken_at: None,
            download_url: None,
            album: None,
            title: None,
            location: None,
            is_favorite: false,
            extra: Default::default(),
        }
    }

    #[tokio::test]
    async fn try_spawn_extend_returns_before_slow_list_photos() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SlowListPlugin {
            delay: Duration::from_secs(5),
        });
        let mut queue_io = QueueIo::new();
        let loader = QueueLoader::new(1);
        let start = Instant::now();
        assert!(queue_io.try_spawn_extend(
            vec![plugin],
            loader,
            HashSet::new(),
            EXTEND_MAX_ROUNDS,
            PAGE_SIZE,
            MAX_PHOTOS_PER_PLUGIN,
        ));
        let drained = queue_io.drain_extend();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "try_spawn_extend+drain took {elapsed:?}"
        );
        assert!(drained.is_none());
        assert!(queue_io.extend_in_flight());
    }

    #[tokio::test]
    async fn drain_extend_drops_stale_generation() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SlowListPlugin {
            delay: Duration::ZERO,
        });
        let mut queue_io = QueueIo::new();
        assert!(queue_io.try_spawn_extend(
            vec![plugin],
            QueueLoader::new(1),
            HashSet::new(),
            EXTEND_MAX_ROUNDS,
            PAGE_SIZE,
            MAX_PHOTOS_PER_PLUGIN,
        ));
        queue_io.invalidate();
        for _ in 0..32 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(
                queue_io.drain_extend().is_none(),
                "stale generation must not surface"
            );
            if !queue_io.extend_in_flight() {
                break;
            }
        }
        assert!(!queue_io.extend_in_flight());
    }

    #[tokio::test]
    async fn try_spawn_favorite_returns_before_slow_set_favorite() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SlowFavoritePlugin {
            delay: Duration::from_secs(5),
        });
        let mut queue_io = QueueIo::new();
        let start = Instant::now();
        assert!(queue_io.try_spawn_favorite(plugin, 0, dummy_meta(), true));
        let drained = queue_io.drain_favorite();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "try_spawn_favorite+drain took {elapsed:?}"
        );
        assert!(drained.is_none());
        assert!(queue_io.fav_in_flight());
    }
}
