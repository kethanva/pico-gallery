//! Background photo fetch + decode so the display loop never awaits plugin I/O.
//!
//! The SDL thread calls [`Fetcher::try_spawn`] (sync) and [`Fetcher::drain`]
//! (sync). Plugin bytes, cache, and JPEG decode run on Tokio tasks. Texture
//! upload stays on the window thread inside `renderer.show_*`.

use crate::cache::CacheHandle;
use crate::renderer::ImageProcessor;
use image::RgbaImage;
use log::warn;
use picogallery_core::{FetchIntent, PhotoMeta, PhotoPlugin};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// One decoding, one downloading. No config knob — the Pi Zero has one core.
pub const MAX_IN_FLIGHT_FETCHES: usize = 2;

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Cache a source-managed thumb only when the payload is small. Never duplicate
/// a multi-megabyte local original merely because the gallery path requested it.
const SOURCE_MANAGED_THUMB_CACHE_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    Slide,
    Thumb,
}

pub enum FetchJob {
    Slide {
        queue_idx: usize,
        plugin_idx: usize,
        meta: PhotoMeta,
        priority: bool,
    },
    Thumb {
        queue_idx: usize,
        plugin_idx: usize,
        meta: PhotoMeta,
        cell_px: u32,
    },
}

impl FetchJob {
    fn kind(&self) -> JobKind {
        match self {
            Self::Slide { .. } => JobKind::Slide,
            Self::Thumb { .. } => JobKind::Thumb,
        }
    }

    fn queue_idx(&self) -> usize {
        match self {
            Self::Slide { queue_idx, .. } | Self::Thumb { queue_idx, .. } => *queue_idx,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    Fetch,
    Decode,
    Timeout,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Slide carries PhotoMeta (~strings); pixels already live on the heap in RgbaImage
pub enum FetchDone {
    Slide {
        generation: u64,
        queue_idx: usize,
        meta: PhotoMeta,
        priority: bool,
        rgba: RgbaImage,
        exif_date: Option<String>,
    },
    Thumb {
        generation: u64,
        queue_idx: usize,
        rgba: RgbaImage,
    },
    Failed {
        generation: u64,
        queue_idx: usize,
        kind: JobKind,
        reason: FailReason,
    },
}

impl FetchDone {
    fn generation(&self) -> u64 {
        match self {
            Self::Slide { generation, .. }
            | Self::Thumb { generation, .. }
            | Self::Failed { generation, .. } => *generation,
        }
    }

    fn pending_key(&self) -> (usize, JobKind) {
        match self {
            Self::Slide { queue_idx, .. } => (*queue_idx, JobKind::Slide),
            Self::Thumb { queue_idx, .. } => (*queue_idx, JobKind::Thumb),
            Self::Failed {
                queue_idx, kind, ..
            } => (*queue_idx, *kind),
        }
    }
}

pub struct Fetcher {
    tx: mpsc::Sender<FetchDone>,
    rx: mpsc::Receiver<FetchDone>,
    plugins: Vec<Arc<dyn PhotoPlugin>>,
    cache: CacheHandle,
    processor: Arc<ImageProcessor>,
    in_flight: usize,
    max_in_flight: usize,
    pending: HashSet<(usize, JobKind)>,
    generation: u64,
}

impl Fetcher {
    pub fn new(
        plugins: Vec<Arc<dyn PhotoPlugin>>,
        cache: CacheHandle,
        processor: Arc<ImageProcessor>,
        max_in_flight: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(8);
        Self {
            tx,
            rx,
            plugins,
            cache,
            processor,
            in_flight: 0,
            max_in_flight: max_in_flight.max(1),
            pending: HashSet::new(),
            generation: 0,
        }
    }

    pub fn has_capacity(&self) -> bool {
        self.in_flight < self.max_in_flight
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    pub fn set_processor(&mut self, processor: Arc<ImageProcessor>) {
        self.processor = processor;
    }

    pub fn set_plugins(&mut self, plugins: Vec<Arc<dyn PhotoPlugin>>) {
        self.plugins = plugins;
        self.invalidate();
    }

    /// Bump generation so in-flight results are dropped on arrival.
    ///
    /// `in_flight` is left alone: stale tasks still occupy the decode budget
    /// until they finish, so a Prev/shuffle cannot stack extra JPEG decodes
    /// on top of the ones already running (MemoryMax=384M).
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
    }

    /// Sync. Never awaits I/O. Returns false when at capacity or already pending.
    pub fn try_spawn(&mut self, job: FetchJob) -> bool {
        if !self.has_capacity() {
            return false;
        }
        let key = (job.queue_idx(), job.kind());
        if !self.pending.insert(key) {
            return false;
        }
        let Some(plugin) = self.plugins.get(job_plugin_idx(&job)).cloned() else {
            // Do not enqueue Failed: drain() always decrements in_flight, and
            // this path never incremented it.
            self.pending.remove(&key);
            warn!(
                "Fetcher: no plugin at index {} for queue {}",
                job_plugin_idx(&job),
                job.queue_idx()
            );
            return false;
        };

        let cache = self.cache.clone();
        let processor = Arc::clone(&self.processor);
        let tx = self.tx.clone();
        let generation = self.generation;
        self.in_flight += 1;
        tokio::spawn(async move {
            let done = run_job(plugin, cache, processor, job, generation).await;
            let _ = tx.send(done).await;
        });
        true
    }

    /// Sync. `try_recv` loop; drops stale-generation results.
    pub fn drain(&mut self) -> Vec<FetchDone> {
        let mut out = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if done.generation() != self.generation {
                continue;
            }
            self.pending.remove(&done.pending_key());
            out.push(done);
        }
        out
    }
}

fn job_plugin_idx(job: &FetchJob) -> usize {
    match job {
        FetchJob::Slide { plugin_idx, .. } | FetchJob::Thumb { plugin_idx, .. } => *plugin_idx,
    }
}

async fn run_job(
    plugin: Arc<dyn PhotoPlugin>,
    cache: CacheHandle,
    processor: Arc<ImageProcessor>,
    job: FetchJob,
    generation: u64,
) -> FetchDone {
    match job {
        FetchJob::Slide {
            queue_idx,
            meta,
            priority,
            ..
        } => {
            let intent = FetchIntent::Fullscreen {
                display_width: processor.width(),
                display_height: processor.height(),
            };
            match fetch_bytes(&*plugin, &cache, &meta, intent).await {
                BytesOutcome::Ok(bytes) => {
                    match tokio::task::spawn_blocking(move || processor.decode_and_scale(&bytes))
                        .await
                    {
                        Ok(Ok((rgba, exif_date))) => FetchDone::Slide {
                            generation,
                            queue_idx,
                            meta,
                            priority,
                            rgba,
                            exif_date,
                        },
                        Ok(Err(e)) => {
                            warn!("Decode error ({}): {e}", meta.filename);
                            cache.remove(&meta.cache_key(plugin.name())).await;
                            FetchDone::Failed {
                                generation,
                                queue_idx,
                                kind: JobKind::Slide,
                                reason: FailReason::Decode,
                            }
                        }
                        Err(e) => {
                            warn!("Decode task failed ({}): {e}", meta.filename);
                            FetchDone::Failed {
                                generation,
                                queue_idx,
                                kind: JobKind::Slide,
                                reason: FailReason::Decode,
                            }
                        }
                    }
                }
                BytesOutcome::Err => FetchDone::Failed {
                    generation,
                    queue_idx,
                    kind: JobKind::Slide,
                    reason: FailReason::Fetch,
                },
                BytesOutcome::Timeout => FetchDone::Failed {
                    generation,
                    queue_idx,
                    kind: JobKind::Slide,
                    reason: FailReason::Timeout,
                },
            }
        }
        FetchJob::Thumb {
            queue_idx,
            meta,
            cell_px,
            ..
        } => {
            let intent = FetchIntent::GalleryThumb { cell_px };
            match fetch_bytes(&*plugin, &cache, &meta, intent).await {
                BytesOutcome::Ok(bytes) => {
                    match tokio::task::spawn_blocking(move || {
                        processor.decode_thumbnail(&bytes, cell_px)
                    })
                    .await
                    {
                        Ok(Ok(rgba)) => FetchDone::Thumb {
                            generation,
                            queue_idx,
                            rgba,
                        },
                        Ok(Err(e)) => {
                            warn!("Thumbnail decode error ({}): {e}", meta.filename);
                            let cache_key =
                                format!("{}/thumb/{cell_px}_{}", plugin.name(), meta.id);
                            cache.remove(&cache_key).await;
                            FetchDone::Failed {
                                generation,
                                queue_idx,
                                kind: JobKind::Thumb,
                                reason: FailReason::Decode,
                            }
                        }
                        Err(e) => {
                            warn!("Thumbnail decode task failed ({}): {e}", meta.filename);
                            FetchDone::Failed {
                                generation,
                                queue_idx,
                                kind: JobKind::Thumb,
                                reason: FailReason::Decode,
                            }
                        }
                    }
                }
                BytesOutcome::Err => FetchDone::Failed {
                    generation,
                    queue_idx,
                    kind: JobKind::Thumb,
                    reason: FailReason::Fetch,
                },
                BytesOutcome::Timeout => FetchDone::Failed {
                    generation,
                    queue_idx,
                    kind: JobKind::Thumb,
                    reason: FailReason::Timeout,
                },
            }
        }
    }
}

enum BytesOutcome {
    Ok(Vec<u8>),
    Err,
    Timeout,
}

async fn fetch_bytes(
    plugin: &dyn PhotoPlugin,
    cache: &CacheHandle,
    meta: &PhotoMeta,
    intent: FetchIntent,
) -> BytesOutcome {
    let plugin_name = plugin.name();
    let uses_engine_cache = plugin.uses_engine_image_cache();
    let cache_key = if intent.is_thumb() {
        format!("{plugin_name}/thumb/{}_{}", intent.target_edge(), meta.id)
    } else {
        meta.cache_key(plugin_name)
    };

    if uses_engine_cache || intent.is_thumb() {
        if let Some(bytes) = cache.get(&cache_key).await {
            if uses_engine_cache || bytes.len() <= SOURCE_MANAGED_THUMB_CACHE_MAX_BYTES {
                return BytesOutcome::Ok(bytes);
            }
            cache.remove(&cache_key).await;
        }
    } else if !intent.is_thumb() {
        // Non-caching plugins skip the disk cache for fullscreen originals.
    }

    let fetch = plugin.get_photo_bytes(meta, intent);
    match tokio::time::timeout(FETCH_TIMEOUT, fetch).await {
        Ok(Ok(bytes)) => {
            // Gallery path only — never substitute an EXIF stub onto a
            // fullscreen LCD, however small (MEMORY.md).
            let bytes = if intent.is_thumb() {
                picogallery_core::prefer_gallery_exif_thumb(bytes, intent.target_edge())
            } else {
                bytes
            };
            if uses_engine_cache
                || (intent.is_thumb() && bytes.len() <= SOURCE_MANAGED_THUMB_CACHE_MAX_BYTES)
            {
                cache.put(&cache_key, &bytes).await;
            }
            BytesOutcome::Ok(bytes)
        }
        Ok(Err(e)) => {
            warn!("fetch {} error: {e}", meta.filename);
            BytesOutcome::Err
        }
        Err(_) => {
            warn!("fetch {} timed out after 30 s", meta.filename);
            BytesOutcome::Timeout
        }
    }
}

/// Prefetch stall bookkeeping, extracted so it is testable without a renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallAction {
    None,
    Warn,
    Osd,
}

pub fn stall_state(
    prefetched_empty: bool,
    in_flight: usize,
    fullscreen: bool,
    elapsed: Option<Duration>,
    warned: bool,
    osd_shown: bool,
) -> StallAction {
    if !fullscreen || !prefetched_empty || in_flight > 0 {
        return StallAction::None;
    }
    let Some(elapsed) = elapsed else {
        return StallAction::None;
    };
    if elapsed >= Duration::from_secs(30) && !osd_shown {
        StallAction::Osd
    } else if elapsed >= Duration::from_secs(10) && !warned {
        StallAction::Warn
    } else {
        StallAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use picogallery_core::{AuthStatus, PluginConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    struct SleepPlugin {
        delay: Duration,
        intents: StdMutex<Vec<FetchIntent>>,
        fetches: AtomicUsize,
    }

    impl SleepPlugin {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                intents: StdMutex::new(Vec::new()),
                fetches: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl PhotoPlugin for SleepPlugin {
        fn name(&self) -> &str {
            "sleep"
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
        async fn get_photo_bytes(&self, _meta: &PhotoMeta, intent: FetchIntent) -> Result<Vec<u8>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.intents.lock().unwrap().push(intent);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
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

    fn test_fetcher(plugin: Arc<dyn PhotoPlugin>, max: usize) -> Fetcher {
        Fetcher::new(
            vec![plugin],
            CacheHandle::disabled(),
            ImageProcessor::for_test(64, 48),
            max,
        )
    }

    #[tokio::test]
    async fn try_spawn_returns_before_a_slow_plugin_completes() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SleepPlugin::new(Duration::from_secs(5)));
        let mut fetcher = test_fetcher(plugin, 2);
        let start = Instant::now();
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        let drained = fetcher.drain();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "try_spawn+drain took {elapsed:?}"
        );
        assert!(drained.is_empty());
        assert_eq!(fetcher.in_flight(), 1);
    }

    #[tokio::test]
    async fn respects_max_in_flight() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SleepPlugin::new(Duration::from_secs(5)));
        let mut fetcher = test_fetcher(plugin, 2);
        // Same queue_idx would dedupe; vary indices.
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 1,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        assert!(!fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 2,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        assert_eq!(fetcher.in_flight(), 2);
    }

    #[tokio::test]
    async fn drain_drops_results_from_a_stale_generation() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SleepPlugin::new(Duration::ZERO));
        let mut fetcher = test_fetcher(plugin, 2);
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        fetcher.invalidate();
        for _ in 0..32 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            let drained = fetcher.drain();
            assert!(
                drained.is_empty(),
                "stale generation must not surface a result"
            );
            if fetcher.in_flight() == 0 {
                break;
            }
        }
        assert_eq!(fetcher.in_flight(), 0);
    }

    #[tokio::test]
    async fn invalidate_does_not_clear_live_pending_on_stale_drain() {
        // Gate the first (stale) fetch so it cannot finish until the live job
        // is spawned — otherwise both complete in one drain and we never see
        // in_flight == 1.
        struct GatedPlugin {
            calls: AtomicUsize,
            release_stale: Arc<tokio::sync::Notify>,
        }
        #[async_trait]
        impl PhotoPlugin for GatedPlugin {
            fn name(&self) -> &str {
                "gated"
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
                _intent: FetchIntent,
            ) -> Result<Vec<u8>> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    self.release_stale.notified().await;
                } else {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
            }
        }

        let release_stale = Arc::new(tokio::sync::Notify::new());
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(GatedPlugin {
            calls: AtomicUsize::new(0),
            release_stale: Arc::clone(&release_stale),
        });
        let mut fetcher = test_fetcher(plugin, 2);
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        fetcher.invalidate();
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        release_stale.notify_one();
        let mut saw_one = false;
        for _ in 0..64 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            let drained = fetcher.drain();
            assert!(
                drained.is_empty() || fetcher.in_flight() == 0,
                "stale generation must not surface while a live job is pending"
            );
            if fetcher.in_flight() == 1 {
                saw_one = true;
                break;
            }
        }
        assert!(
            saw_one,
            "stale job should complete while live job is in flight"
        );
        assert!(
            !fetcher.try_spawn(FetchJob::Slide {
                queue_idx: 0,
                plugin_idx: 0,
                meta: dummy_meta(),
                priority: false,
            }),
            "live pending bit must survive draining a stale completion of the same idx"
        );
    }

    #[tokio::test]
    async fn thumb_job_uses_gallery_intent_and_slide_job_uses_fullscreen() {
        let plugin = Arc::new(SleepPlugin::new(Duration::ZERO));
        let recorded = Arc::clone(&plugin);
        let mut fetcher = test_fetcher(plugin, 2);
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        assert!(fetcher.try_spawn(FetchJob::Thumb {
            queue_idx: 1,
            plugin_idx: 0,
            meta: dummy_meta(),
            cell_px: 140,
        }));
        for _ in 0..64 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = fetcher.drain();
            if fetcher.in_flight() == 0 {
                break;
            }
        }
        let intents = recorded.intents.lock().unwrap().clone();
        assert!(
            intents.iter().any(|i| matches!(
                i,
                FetchIntent::Fullscreen {
                    display_width: 64,
                    display_height: 48
                }
            )),
            "slide job must pass Fullscreen, got {intents:?}"
        );
        assert!(
            intents
                .iter()
                .any(|i| matches!(i, FetchIntent::GalleryThumb { cell_px: 140 })),
            "thumb job must pass GalleryThumb, got {intents:?}"
        );
        assert!(intents.iter().all(|i| {
            if let FetchIntent::Fullscreen {
                display_width,
                display_height,
            } = i
            {
                !i.is_thumb() && (*display_width, *display_height) == (64, 48)
            } else {
                i.is_thumb()
            }
        }));
    }

    #[tokio::test]
    async fn cache_disabled_handle_still_serves_bytes() {
        let plugin: Arc<dyn PhotoPlugin> = Arc::new(SleepPlugin::new(Duration::ZERO));
        let mut fetcher = test_fetcher(plugin, 1);
        assert!(!fetcher.cache.is_enabled());
        assert!(fetcher.try_spawn(FetchJob::Slide {
            queue_idx: 0,
            plugin_idx: 0,
            meta: dummy_meta(),
            priority: false,
        }));
        let mut got = false;
        for _ in 0..64 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            for done in fetcher.drain() {
                // Decode of a 4-byte SOI/EOI may fail; either Slide or Failed
                // (decode) still proves the plugin was reached without a cache.
                match done {
                    FetchDone::Slide { .. }
                    | FetchDone::Thumb { .. }
                    | FetchDone::Failed { .. } => got = true,
                }
            }
            if got {
                break;
            }
        }
        assert!(got, "disabled cache must not block a fetch");
    }

    #[test]
    fn stall_detector_warns_once_per_episode() {
        assert_eq!(
            stall_state(true, 0, true, Some(Duration::from_secs(10)), false, false),
            StallAction::Warn
        );
        assert_eq!(
            stall_state(true, 0, true, Some(Duration::from_secs(10)), true, false),
            StallAction::None
        );
        assert_eq!(
            stall_state(true, 0, true, Some(Duration::from_secs(30)), true, false),
            StallAction::Osd
        );
        assert_eq!(
            stall_state(true, 1, true, Some(Duration::from_secs(30)), false, false),
            StallAction::None
        );
        assert_eq!(
            stall_state(false, 0, true, Some(Duration::from_secs(30)), false, false),
            StallAction::None
        );
    }
}
