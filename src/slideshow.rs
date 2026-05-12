/// Slideshow engine.
///
/// Runs on the Tokio runtime. A background task pre-fetches the next N images
/// while the current one is on screen, so transitions are instant on slow Pi
/// Zero I/O.  All plugin calls are async and non-blocking.
use anyhow::Result;
use image::{Rgba, RgbaImage};
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

        use crate::config::PhotoOrder;
        match self.config.display.order {
            PhotoOrder::Shuffle => {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(42);
                shuffle(&mut all, seed);
                info!("Photo order: shuffle ({} photos)", all.len());
            }
            PhotoOrder::Chronological => {
                all.sort_by_key(|(_, m)| m.taken_at);
                info!("Photo order: chronological ({} photos)", all.len());
            }
            PhotoOrder::NewestFirst => {
                all.sort_by(|(_, a), (_, b)| b.taken_at.cmp(&a.taken_at));
                info!("Photo order: newest first ({} photos)", all.len());
            }
        }

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
        let mut prefetched: VecDeque<(usize, usize, PhotoMeta, Vec<u8>)> = VecDeque::new();
        let mut current_queue_idx = 0usize;
        let mut current_rgba: Option<RgbaImage> = None;
        let mut paused = false;
        // Tracks whether the display is currently powered on so we emit
        // vcgencmd and the black frame only at the exact on→off / off→on edges.
        let mut display_was_on = true;

        // Pre-warm the prefetch queue.
        for i in 0..prefetch_n.min(queue.len()) {
            if let Some(bytes) = self.fetch_photo(&queue[i].0, &queue[i].1, renderer).await {
                prefetched.push_back((i, queue[i].0, queue[i].1.clone(), bytes));
            }
        }
        let mut cursor = prefetch_n.min(queue.len());

        let slide_dur = Duration::from_secs(self.config.display.slide_duration_secs);
        let trans_dur = Duration::from_millis(self.config.display.transition_ms as u64);

        // Initialize last_advance so that the first photo shows immediately
        let mut last_advance = Instant::now().checked_sub(slide_dur).unwrap_or_else(Instant::now);

        loop {
            // ── Event handling ─────────────────────────────────────────────
            if let Some(cmd) = renderer.poll_events() {
                match cmd {
                    SlideshowCmd::Quit => {
                        info!("Quit requested.");
                        self.cache.lock().await.flush().await;
                        return Ok(());
                    }
                    SlideshowCmd::TogglePause => {
                        paused = !paused;
                        info!("Slideshow {}.", if paused { "paused" } else { "resumed" });
                        last_advance = Instant::now();
                    }
                    SlideshowCmd::Next => {
                        last_advance = Instant::now().checked_sub(slide_dur).unwrap_or_else(Instant::now); // force advance
                    }
                    SlideshowCmd::Prev => {
                        current_queue_idx = if current_queue_idx == 0 {
                            queue.len().saturating_sub(1)
                        } else {
                            current_queue_idx - 1
                        };
                        prefetched.clear();
                        cursor = current_queue_idx;
                        last_advance = Instant::now().checked_sub(slide_dur).unwrap_or_else(Instant::now);
                    }
                }
            }

            // ── Display schedule ───────────────────────────────────────────
            //
            // When the schedule says the display should be off:
            //  1. Render a black frame once (at the off-edge transition).
            //  2. Ask vcgencmd to cut HDMI power (Pi only; silent no-op elsewhere).
            //  3. Sleep cheaply — still polling events so Quit is always handled.
            // When the schedule says the display should come back on:
            //  1. Ask vcgencmd to restore HDMI power.
            //  2. Force an immediate photo advance so content appears at once.
            if !self.config.display.schedule_active_now() {
                if display_was_on {
                    let w = renderer.width().max(1);
                    let h = renderer.height().max(1);
                    let black = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
                    if let Err(e) = renderer.show_cut(&black) {
                        warn!("schedule: could not show black frame: {e}");
                    }
                    crate::display_power::set_power(false).await;
                    display_was_on = false;
                    info!("Display schedule: display off.");
                }
                // 1-second sleep keeps the loop responsive without burning CPU.
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            if !display_was_on {
                crate::display_power::set_power(true).await;
                display_was_on = true;
                // Force immediate photo advance so the screen doesn't stay black.
                last_advance = Instant::now()
                    .checked_sub(slide_dur)
                    .unwrap_or_else(Instant::now);
                info!("Display schedule: display on.");
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
            if let Some((q_idx, _pidx, meta, bytes)) = prefetched.pop_front() {
                debug!("Showing: {}", meta.filename);
                match renderer.decode_and_scale(&bytes) {
                    Ok((mut rgba, exif_date)) => {
                        // Stamp metadata overlay before handing to the transition.
                        // exif_date comes from the same EXIF parse that corrected
                        // orientation — no second parse needed.
                        if self.config.display.show_osd {
                            crate::osd::draw_photo_info(
                                &mut rgba,
                                &meta,
                                exif_date.as_deref(),
                            );
                        }
                        let result = match self.config.display.transition {
                            Transition::Cut => renderer.show_cut(&rgba),
                            Transition::Fade => renderer.show_fade(current_rgba.as_ref(), &rgba, trans_dur),
                            Transition::SlideLeft => renderer.show_slide_left(current_rgba.as_ref(), &rgba, trans_dur),
                            Transition::SlideRight => renderer.show_slide_left(current_rgba.as_ref(), &rgba, trans_dur),
                        };
                        if let Err(e) = result { warn!("Render error: {}", e); }
                        current_queue_idx = q_idx;
                        current_rgba = Some(rgba);
                        last_advance = Instant::now();
                    }
                    Err(e) => { warn!("Decode error ({}): {}", meta.filename, e); }
                }
            }

            // ── Prefetch the next photo in background ─────────────────────
            // Only fetch if the buffer isn't already full (strict size enforcement).
            if cursor < queue.len() && prefetched.len() < prefetch_n {
                let (pidx, meta) = &queue[cursor];
                if let Some(bytes) = self.fetch_photo(pidx, meta, renderer).await {
                    prefetched.push_back((cursor, *pidx, meta.clone(), bytes));
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

        // Fetch from remote — 30 s timeout prevents a hung plugin from stalling the slideshow.
        let fetch = plugin.get_photo_bytes(meta, renderer.width(), renderer.height());
        match tokio::time::timeout(Duration::from_secs(30), fetch).await {
            Ok(Ok(bytes)) => {
                let _ = self.cache.lock().await.put(&cache_key, &bytes).await;
                Some(bytes)
            }
            Ok(Err(e)) => {
                warn!("fetch_photo {} error: {}", meta.filename, e);
                None
            }
            Err(_) => {
                warn!("fetch_photo {} timed out after 30 s", meta.filename);
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
