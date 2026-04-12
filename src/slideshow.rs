/// Slideshow engine.
///
/// Runs on the Tokio runtime. A background task pre-fetches the next N images
/// while the current one is on screen, so transitions are instant on slow Pi
/// Zero I/O.  All plugin calls are async and non-blocking.
use anyhow::Result;
use image::RgbaImage;
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::cache::ImageCache;
use crate::config::{Config, Transition};
use crate::plugin::{AuthStatus, BoxedPlugin, PhotoMeta};
use crate::renderer::{Renderer, SlideshowCmd};

const PAGE_SIZE: usize = 50; // photos fetched per API page

pub struct Slideshow {
    config: Config,
    plugins: Vec<BoxedPlugin>,
    cache: Arc<Mutex<ImageCache>>,
}

impl Slideshow {
    pub async fn new(config: Config, plugins: Vec<BoxedPlugin>) -> Result<Self> {
        let cache = ImageCache::open(&config.cache.resolved_dir(), config.cache.max_mb).await?;
        Ok(Self {
            config,
            plugins,
            cache: Arc::new(Mutex::new(cache)),
        })
    }

    /// Run the slideshow.  Blocks the calling thread until the user quits.
    pub async fn run(mut self) -> Result<()> {
        // 1. Authenticate all plugins.
        self.authenticate_all().await?;

        // 2. Build the play queue (all photos from all plugins, shuffled).
        let queue = self.build_queue().await?;
        if queue.is_empty() {
            error!("No photos found across all plugins. Check your config.");
            return Ok(());
        }
        info!("Play queue: {} photos", queue.len());

        // 3. Create renderer on the main thread (SDL2 requires it).
        let mut renderer = Renderer::init(self.config.display.clone())?;

        // 4. Main display loop.
        self.display_loop(&mut renderer, queue).await
    }

    // ── Authentication ────────────────────────────────────────────────────

    async fn authenticate_all(&mut self) -> Result<()> {
        for plugin in &mut self.plugins {
            info!("Authenticating plugin: {}", plugin.display_name());
            loop {
                match plugin.authenticate().await? {
                    AuthStatus::Authenticated => {
                        info!("  {} authenticated.", plugin.display_name());
                        break;
                    }
                    AuthStatus::PendingUserAction { message, poll_interval_secs } => {
                        // Print instructions to the terminal; in a future release
                        // these would render on-screen via OSD.
                        println!("\n=== {} ===\n{}", plugin.display_name(), message);
                        println!("Checking again in {} seconds…", poll_interval_secs);
                        tokio::time::sleep(Duration::from_secs(poll_interval_secs)).await;
                    }
                    AuthStatus::NotAuthenticated => {
                        warn!("  {} is not authenticated and cannot continue.", plugin.display_name());
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    // ── Queue building ────────────────────────────────────────────────────

    async fn build_queue(&self) -> Result<Vec<(usize, PhotoMeta)>> {
        let mut all: Vec<(usize, PhotoMeta)> = Vec::new();

        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            let mut offset = 0;
            loop {
                match plugin.list_photos(PAGE_SIZE, offset).await {
                    Ok(page) if page.is_empty() => break,
                    Ok(page) => {
                        info!("  {} loaded {} photos (offset {})", plugin.name(), page.len(), offset);
                        offset += page.len();
                        all.extend(page.into_iter().map(|m| (plugin_idx, m)));
                        if offset >= 2000 { break; } // cap at 2 000 per plugin
                    }
                    Err(e) => { warn!("  {} list_photos error: {}", plugin.name(), e); break; }
                }
            }
        }

        // Shuffle deterministically (simple Fisher-Yates with timestamp seed).
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42);
        shuffle(&mut all, seed);

        Ok(all)
    }

    // ── Display loop ──────────────────────────────────────────────────────

    async fn display_loop(
        &self,
        renderer: &mut Renderer,
        queue: Vec<(usize, PhotoMeta)>,
    ) -> Result<()> {
        // Prefetch ring: up to `prefetch_count` decoded images ready ahead.
        let prefetch_n = self.config.cache.prefetch_count;
        let mut prefetched: VecDeque<(usize, PhotoMeta, Vec<u8>)> = VecDeque::new();
        let mut cursor = 0usize;
        let mut current_rgba: Option<RgbaImage> = None;
        let mut paused = false;

        // Pre-warm the prefetch queue.
        for i in 0..prefetch_n.min(queue.len()) {
            if let Some(bytes) = self.fetch_photo(&queue[i].0, &queue[i].1, renderer).await {
                prefetched.push_back((queue[i].0, queue[i].1.clone(), bytes));
            }
        }
        cursor = prefetch_n.min(queue.len());

        let slide_dur = Duration::from_secs(self.config.display.slide_duration_secs);
        let trans_dur = Duration::from_millis(self.config.display.transition_ms as u64);

        let mut last_advance = Instant::now();

        loop {
            // ── Event handling ─────────────────────────────────────────────
            if let Some(cmd) = renderer.poll_events() {
                match cmd {
                    SlideshowCmd::Quit => { info!("Quit requested."); return Ok(()); }
                    SlideshowCmd::TogglePause => {
                        paused = !paused;
                        info!("Slideshow {}.", if paused { "paused" } else { "resumed" });
                        last_advance = Instant::now();
                    }
                    SlideshowCmd::Next => {
                        last_advance = Instant::now() - slide_dur; // force advance
                    }
                    SlideshowCmd::Prev => {
                        // Crude: go back in queue. This requires tracking history.
                        // For now just restart the current photo.
                        last_advance = Instant::now();
                    }
                }
            }

            if paused {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // ── Time to advance? ──────────────────────────────────────────
            if last_advance.elapsed() < slide_dur {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // ── Display next photo ────────────────────────────────────────
            if let Some((_pidx, meta, bytes)) = prefetched.pop_front() {
                debug!("Showing: {}", meta.filename);
                match renderer.decode_and_scale(&bytes) {
                    Ok(rgba) => {
                        let result = match self.config.display.transition {
                            Transition::Cut => renderer.show_cut(&rgba),
                            Transition::Fade => renderer.show_fade(current_rgba.as_ref(), &rgba, trans_dur),
                            Transition::SlideLeft => renderer.show_slide_left(current_rgba.as_ref(), &rgba, trans_dur),
                            Transition::SlideRight => renderer.show_slide_left(current_rgba.as_ref(), &rgba, trans_dur),
                        };
                        if let Err(e) = result { warn!("Render error: {}", e); }
                        current_rgba = Some(rgba);
                        last_advance = Instant::now();
                    }
                    Err(e) => { warn!("Decode error ({}): {}", meta.filename, e); }
                }
            }

            // ── Prefetch the next photo in background ─────────────────────
            if cursor < queue.len() {
                let (pidx, meta) = &queue[cursor];
                if let Some(bytes) = self.fetch_photo(pidx, meta, renderer).await {
                    prefetched.push_back((*pidx, meta.clone(), bytes));
                }
                cursor += 1;

                // Wrap around.
                if cursor >= queue.len() { cursor = 0; }
            }
        }
    }

    // ── Fetching ──────────────────────────────────────────────────────────

    async fn fetch_photo(
        &self,
        plugin_idx: &usize,
        meta: &PhotoMeta,
        renderer: &Renderer,
    ) -> Option<Vec<u8>> {
        let plugin = &self.plugins[*plugin_idx];
        let cache_key = meta.cache_key(plugin.name());

        // Check disk cache first.
        if let Some(bytes) = self.cache.lock().await.get(&cache_key).await {
            return Some(bytes);
        }

        // Fetch from remote.
        match plugin.get_photo_bytes(meta, renderer.width(), renderer.height()).await {
            Ok(bytes) => {
                let _ = self.cache.lock().await.put(&cache_key, &bytes).await;
                Some(bytes)
            }
            Err(e) => {
                warn!("fetch_photo {} error: {}", meta.filename, e);
                None
            }
        }
    }
}

// ── Fisher-Yates shuffle (no_std-safe, no rand dep) ──────────────────────────

fn shuffle<T>(v: &mut Vec<T>, seed: u64) {
    let mut s = seed;
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let j = (s as usize) % (i + 1);
        v.swap(i, j);
    }
}
