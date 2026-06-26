/// Slideshow engine.
///
/// Runs on the Tokio runtime. A background task pre-fetches the next N images
/// while the current one is on screen, so transitions are instant on slow Pi
/// Zero I/O.  All plugin calls are async and non-blocking.
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::cache::ImageCache;
use crate::config::{Config, DisplayConfig, PhotoOrder, Transition};
use crate::menu::{Menu, MenuAction};
use crate::plugin::{AuthStatus, BoxedPlugin, PhotoMeta};
use crate::remote::SharedStatus;
use crate::renderer::{Renderer, SlideshowCmd};
use tokio::sync::mpsc::Receiver;

const PAGE_SIZE: usize = 50; // photos fetched per API page

/// Settings-menu title. Used both to render the panel and to compute its
/// geometry for click/hover hit-testing, so the two must use the same string.
const MENU_TITLE: &str = "PicoGallery - Settings";

/// Builds fresh plugin instances from a config. Lets the engine rebuild its
/// photo sources at runtime (e.g. when the user switches source from the
/// menu) without the slideshow needing to know which plugins were compiled in
/// — the menu therefore works for any package/extension.
pub type PluginFactory = Box<dyn Fn(&Config) -> Vec<BoxedPlugin>>;

pub struct Slideshow {
    config: Config,
    plugins: Vec<BoxedPlugin>,
    cache: Arc<Mutex<ImageCache>>,
    /// Where to persist settings when the user picks "Save settings".
    config_path: PathBuf,
    /// Rebuilds the plugin set from a config — used to switch source at runtime.
    factory: PluginFactory,
}

/// How the display loop must react to a menu action.
enum MenuOutcome {
    /// Nothing structural changed — just repaint the menu.
    Stay,
    /// User chose Exit.
    Quit,
    /// A decode-affecting setting changed; drop pre-decoded frames so the next
    /// ones pick up the new setting.
    ReloadFrames,
    /// The play order changed; install the re-ordered queue.
    NewQueue(Vec<(usize, PhotoMeta)>),
    /// The photo source changed; install the new queue and close the menu.
    Switched(Vec<(usize, PhotoMeta)>),
}

impl Slideshow {
    pub async fn new(
        config: Config,
        plugins: Vec<BoxedPlugin>,
        config_path: PathBuf,
        factory: PluginFactory,
    ) -> Result<Self> {
        let cache = ImageCache::open(&config.cache.resolved_dir(), config.cache.max_mb).await?;
        Ok(Self {
            config,
            plugins,
            cache: Arc::new(Mutex::new(cache)),
            config_path,
            factory,
        })
    }

    /// Run the slideshow.  Blocks the calling thread until the user quits.
    ///
    /// `remote_rx` / `remote_status` come from `remote::start` when the HTTP
    /// remote is enabled; both are `None` otherwise.
    pub async fn run(
        mut self,
        remote_rx: Option<Receiver<SlideshowCmd>>,
        remote_status: Option<SharedStatus>,
    ) -> Result<()> {
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
        self.display_loop(&mut renderer, queue, remote_rx, remote_status)
            .await
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
                    AuthStatus::PendingUserAction {
                        message,
                        poll_interval_secs,
                    } => {
                        // Print instructions to the terminal; in a future release
                        // these would render on-screen via OSD.
                        println!("\n=== {} ===\n{}", plugin.display_name(), message);
                        println!("Checking again in {} seconds…", poll_interval_secs);
                        tokio::time::sleep(Duration::from_secs(poll_interval_secs)).await;
                    }
                    AuthStatus::NotAuthenticated => {
                        warn!(
                            "  {} is not authenticated and cannot continue.",
                            plugin.display_name()
                        );
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    // ── Queue building ────────────────────────────────────────────────────

    async fn build_queue(&self) -> Result<Vec<(usize, PhotoMeta)>> {
        Self::build_queue_with(&self.plugins, &self.config.display).await
    }

    /// Build the shuffled/ordered play queue from an explicit plugin set and
    /// display config. Used both at startup and when the source is switched at
    /// runtime — keeping it free of `&self` lets the switch path build a queue
    /// from a fresh, not-yet-installed plugin set before committing to it.
    async fn build_queue_with(
        plugins: &[BoxedPlugin],
        display: &DisplayConfig,
    ) -> Result<Vec<(usize, PhotoMeta)>> {
        let mut all: Vec<(usize, PhotoMeta)> = Vec::new();

        for (plugin_idx, plugin) in plugins.iter().enumerate() {
            let mut offset = 0;
            loop {
                match plugin.list_photos(PAGE_SIZE, offset).await {
                    Ok(page) if page.is_empty() => break,
                    Ok(page) => {
                        info!(
                            "  {} loaded {} photos (offset {})",
                            plugin.name(),
                            page.len(),
                            offset
                        );
                        offset += page.len();
                        all.extend(page.into_iter().map(|m| (plugin_idx, m)));
                        if offset >= 2000 {
                            break;
                        } // cap at 2 000 per plugin
                    }
                    Err(e) => {
                        warn!("  {} list_photos error: {}", plugin.name(), e);
                        break;
                    }
                }
            }
        }

        // Nanosecond clock as shuffle seed. On an RTC-less Pi that cold-boots
        // before NTP sync the clock (and therefore the seed) is roughly the
        // same every boot — including the literal-42 fallback if the clock
        // sits before the epoch — so the shuffle order repeats until time
        // syncs. Cosmetic only; not worth an entropy source.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42);

        match display.order {
            PhotoOrder::Shuffle => {
                shuffle(&mut all, seed);
                if display.on_this_day_boost {
                    weave_on_this_day(&mut all);
                }
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
            PhotoOrder::DateCluster => {
                all = date_cluster_order(all, seed);
                info!("Photo order: date clusters ({} photos)", all.len());
            }
        }

        Ok(all)
    }

    // ── Display loop ──────────────────────────────────────────────────────

    async fn display_loop(
        &mut self,
        renderer: &mut Renderer,
        mut queue: Vec<(usize, PhotoMeta)>,
        mut remote_rx: Option<Receiver<SlideshowCmd>>,
        remote_status: Option<SharedStatus>,
    ) -> Result<()> {
        // Prefetch ring: up to `prefetch_count` photos fetched *and* fully
        // decoded/scaled ahead of time, so showing a slide is just a texture
        // upload + transition — the costly JPEG decode and Lanczos resize run
        // during the idle window, off the transition-start critical path.
        // Each entry: (queue index, metadata, display-ready RGBA, EXIF date).
        // Clamp to ≥1: a ring capacity of 0 would make every prefetch a no-op,
        // so nothing would ever be decoded or shown.
        let prefetch_n = self.config.cache.prefetch_count.max(1);
        let mut prefetched: VecDeque<(usize, PhotoMeta, RgbaImage, Option<String>)> =
            VecDeque::new();
        let mut current_queue_idx = 0usize;
        let mut current_rgba: Option<RgbaImage> = None;
        // The on-screen photo's plugin index + metadata, so the favourite
        // toggle knows which plugin to call and can flip the local state.
        let mut current_meta: Option<(usize, PhotoMeta)> = None;
        let mut paused = false;
        // Tracks whether the display is currently powered on so we emit
        // vcgencmd and the black frame only at the exact on→off / off→on edges.
        let mut display_was_on = true;

        // Right-click settings menu (config + source switch + exit). Closed by
        // default; rendered only while open and only when its state changes, so
        // it adds no steady-state CPU or memory cost on a Pi Zero.
        let mut menu = Menu::default();
        let mut menu_dirty = false;

        // Pre-warm the prefetch ring (fetch + decode the first N photos).
        let mut cursor = 0usize;
        for _ in 0..prefetch_n {
            self.prefetch_one(&queue, &mut cursor, &mut prefetched, prefetch_n, renderer)
                .await;
        }

        let slide_dur = Duration::from_secs(self.config.display.slide_duration_secs);

        // Initialize last_advance so that the first photo shows immediately
        let mut last_advance = Instant::now()
            .checked_sub(slide_dur)
            .unwrap_or_else(Instant::now);

        loop {
            // Re-read timing each iteration so menu changes to slide duration /
            // transition take effect immediately (both are trivially cheap).
            let slide_dur = Duration::from_secs(self.config.display.slide_duration_secs);
            let trans_dur = Duration::from_millis(self.config.display.transition_ms as u64);

            // ── Event handling ─────────────────────────────────────────────
            // Keyboard/mouse first, then everything the HTTP remote queued —
            // both sources flow through the same match below. poll_events
            // interprets input differently while the menu is open, so it gets
            // the current menu state.
            let mut cmds: Vec<SlideshowCmd> = renderer.poll_events(menu.open);
            if let Some(rx) = remote_rx.as_mut() {
                while let Ok(cmd) = rx.try_recv() {
                    cmds.push(cmd);
                }
            }

            // Menu rows + labels for *this* input batch, built once so clicks
            // map to the exact layout poll_events hit-tested against. Only built
            // when the menu is open *and* there is input to interpret — an
            // idle-open menu allocates nothing each tick (Pi Zero stays cold).
            let (action_rows, action_labels) = if menu.open && !cmds.is_empty() {
                let sources = self.source_list();
                let rows = crate::menu::build_rows(&self.config.display, paused, &sources);
                let labels: Vec<String> = rows.iter().map(|r| r.label.clone()).collect();
                (rows, labels)
            } else {
                (Vec::new(), Vec::new())
            };
            // At most one activation per batch — users don't double-click inside
            // a 30 ms tick, and this keeps the borrow-heavy action handling out
            // of the per-command match.
            let mut pending_activate = false;

            for cmd in cmds {
                match cmd {
                    SlideshowCmd::Quit => {
                        info!("Quit requested.");
                        self.cache.lock().await.flush().await;
                        return Ok(());
                    }
                    SlideshowCmd::OpenMenu => {
                        menu.open = true;
                        menu.selected = 0;
                        menu_dirty = true;
                    }
                    SlideshowCmd::CloseMenu => {
                        if menu.open {
                            menu.open = false;
                            // Repaint the photo underneath so the menu vanishes.
                            if let Some(img) = &current_rgba {
                                let _ = renderer.show_cut(img);
                            }
                            if !paused {
                                last_advance = Instant::now();
                            }
                        }
                    }
                    SlideshowCmd::MenuMove(d) => {
                        if menu.open && !action_rows.is_empty() {
                            let n = action_rows.len() as i32;
                            menu.selected = (((menu.selected as i32 + d) % n + n) % n) as usize;
                            menu_dirty = true;
                        }
                    }
                    SlideshowCmd::MenuPoint { x, y } => {
                        if menu.open {
                            if let Some(idx) = crate::osd::menu_hit_test(
                                renderer.width(),
                                renderer.height(),
                                MENU_TITLE,
                                &action_labels,
                                x,
                                y,
                            ) {
                                if idx != menu.selected {
                                    menu.selected = idx;
                                    menu_dirty = true;
                                }
                            }
                        }
                    }
                    SlideshowCmd::MenuClick { x, y } => {
                        if menu.open {
                            match crate::osd::menu_hit_test(
                                renderer.width(),
                                renderer.height(),
                                MENU_TITLE,
                                &action_labels,
                                x,
                                y,
                            ) {
                                Some(idx) => {
                                    menu.selected = idx;
                                    pending_activate = true;
                                }
                                // Click outside the panel dismisses the menu.
                                None => {
                                    menu.open = false;
                                    if let Some(img) = &current_rgba {
                                        let _ = renderer.show_cut(img);
                                    }
                                    if !paused {
                                        last_advance = Instant::now();
                                    }
                                }
                            }
                        }
                    }
                    SlideshowCmd::MenuActivate => {
                        if menu.open {
                            pending_activate = true;
                        }
                    }
                    // Normal slideshow commands are ignored while the menu is
                    // up (the photo behind it isn't advancing anyway).
                    SlideshowCmd::TogglePause => {
                        if !menu.open {
                            paused = !paused;
                            info!("Slideshow {}.", if paused { "paused" } else { "resumed" });
                            last_advance = Instant::now();
                            if let Some(status) = &remote_status {
                                status.lock().unwrap_or_else(|e| e.into_inner()).paused = paused;
                            }
                        }
                    }
                    SlideshowCmd::Next => {
                        if !menu.open {
                            last_advance = Instant::now()
                                .checked_sub(slide_dur)
                                .unwrap_or_else(Instant::now); // force advance
                        }
                    }
                    SlideshowCmd::Prev => {
                        if !menu.open {
                            current_queue_idx = if current_queue_idx == 0 {
                                queue.len().saturating_sub(1)
                            } else {
                                current_queue_idx - 1
                            };
                            prefetched.clear();
                            cursor = current_queue_idx;
                            last_advance = Instant::now()
                                .checked_sub(slide_dur)
                                .unwrap_or_else(Instant::now);
                        }
                    }
                    SlideshowCmd::ToggleFavorite => {
                        if !menu.open {
                            self.toggle_favorite(&mut current_meta, &remote_status)
                                .await;
                        }
                    }
                }
            }

            // ── Apply a menu activation (at most one per input batch) ───────
            if pending_activate && menu.open && !action_rows.is_empty() {
                let sel = menu.selected.min(action_rows.len() - 1);
                match action_rows[sel].action {
                    // Pause/Resume shares the slideshow's pause flag, a loop
                    // local — handle it here rather than in the action helper.
                    MenuAction::TogglePause => {
                        paused = !paused;
                        info!("Slideshow {}.", if paused { "paused" } else { "resumed" });
                        last_advance = Instant::now();
                        if let Some(status) = &remote_status {
                            status.lock().unwrap_or_else(|e| e.into_inner()).paused = paused;
                        }
                        menu_dirty = true;
                    }
                    other => match self.handle_menu_action(other, renderer).await {
                        MenuOutcome::Stay => menu_dirty = true,
                        MenuOutcome::Quit => {
                            self.cache.lock().await.flush().await;
                            return Ok(());
                        }
                        MenuOutcome::ReloadFrames => {
                            prefetched.clear();
                            if !queue.is_empty() {
                                cursor = (current_queue_idx + 1) % queue.len();
                            }
                            menu_dirty = true;
                        }
                        MenuOutcome::NewQueue(q) => {
                            queue = q;
                            cursor = 0;
                            current_queue_idx = 0;
                            prefetched.clear();
                            menu_dirty = true;
                        }
                        MenuOutcome::Switched(q) => {
                            queue = q;
                            cursor = 0;
                            current_queue_idx = 0;
                            prefetched.clear();
                            current_meta = None;
                            menu.open = false;
                            // Show the new source promptly.
                            last_advance = Instant::now()
                                .checked_sub(slide_dur)
                                .unwrap_or_else(Instant::now);
                        }
                    },
                }
            }

            // ── Menu overlay ───────────────────────────────────────────────
            // While the menu is open the slideshow does not advance and nothing
            // is prefetched or decoded — the only work is repainting the panel,
            // and only when something changed. A Pi Zero stays idle here.
            if menu.open {
                if menu_dirty {
                    let sources = self.source_list();
                    let labels: Vec<String> =
                        crate::menu::build_rows(&self.config.display, paused, &sources)
                            .into_iter()
                            .map(|r| r.label)
                            .collect();
                    // Composite onto a full-screen base so the menu geometry is
                    // in screen coordinates. Otherwise click/hover hit-testing
                    // (which uses the screen height) would be vertically offset
                    // whenever the photo doesn't fill the screen — e.g. plain
                    // letterbox (`letterbox_blur = false`) or `fill_screen` crop.
                    let mut frame = RgbaImage::from_pixel(
                        renderer.width().max(1),
                        renderer.height().max(1),
                        Rgba([0, 0, 0, 255]),
                    );
                    if let Some(img) = &current_rgba {
                        blit_center(&mut frame, img);
                    }
                    crate::osd::draw_menu(&mut frame, MENU_TITLE, &labels, menu.selected);
                    if let Err(e) = renderer.show_cut(&frame) {
                        warn!("menu render error: {e}");
                    }
                    menu_dirty = false;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                continue;
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
            // Not yet — spend the idle window topping up the prefetch buffer.
            // This is where read-ahead actually earns its keep: refilling here
            // (instead of only right after an advance) keeps the next image
            // decoded-and-ready in RAM through the whole on-screen period, and
            // a failed fetch is retried within ~50 ms rather than one slot per
            // slide. When the buffer is already full this is a cheap no-op.
            if last_advance.elapsed() < slide_dur {
                self.prefetch_one(&queue, &mut cursor, &mut prefetched, prefetch_n, renderer)
                    .await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // ── Display next photo ────────────────────────────────────────
            // The image is already decoded and scaled (done in prefetch_one),
            // so all that's left are the cheap per-slide pixel passes and the
            // transition itself.
            if let Some((q_idx, meta, mut rgba, exif_date)) = prefetched.pop_front() {
                debug!("Showing: {}", meta.filename);
                // Night window: dim + warm-shift the photo once per slide
                // (single pixel pass — never per frame). Evaluated at *display*
                // time, not prefetch time, so it tracks the wall clock. Applied
                // before the OSD so the overlay stays readable.
                if self.config.display.night_active_now() {
                    crate::night::apply_night(
                        &mut rgba,
                        self.config.display.night_dim_percent,
                        self.config.display.night_warmth,
                    );
                }
                // Stamp metadata overlay before handing to the transition.
                // exif_date comes from the same EXIF parse that corrected
                // orientation during prefetch — no second parse needed.
                if self.config.display.show_osd {
                    crate::osd::draw_photo_info(&mut rgba, &meta, exif_date.as_deref());
                    crate::osd::draw_nav_arrows(&mut rgba);
                    // Mark already-favourited photos with a ♥ in the corner.
                    if meta
                        .extra
                        .get("favorite")
                        .map(|v| v == "true")
                        .unwrap_or(false)
                    {
                        crate::osd::draw_favorite(&mut rgba);
                    }
                }
                // Clock is its own toggle (independent of show_osd). Formatted
                // here from the wall clock at display time, so it reflects the
                // current minute each slide.
                if self.config.display.show_clock {
                    let now = chrono::Local::now().format("%H:%M").to_string();
                    crate::osd::draw_clock(&mut rgba, &now);
                }
                let result = match self.config.display.transition {
                    Transition::Cut => renderer.show_cut(&rgba),
                    Transition::Fade => {
                        renderer
                            .show_fade(current_rgba.as_ref(), &rgba, trans_dur)
                            .await
                    }
                    Transition::SlideLeft => {
                        renderer
                            .show_slide_left(current_rgba.as_ref(), &rgba, trans_dur)
                            .await
                    }
                    Transition::SlideRight => {
                        renderer
                            .show_slide_right(current_rgba.as_ref(), &rgba, trans_dur)
                            .await
                    }
                };
                if let Err(e) = result {
                    warn!("Render error: {}", e);
                }
                current_queue_idx = q_idx;
                current_rgba = Some(rgba);
                // Remember the source plugin + metadata for the favourite toggle.
                let plugin_idx = queue[q_idx].0;
                let favorite = meta
                    .extra
                    .get("favorite")
                    .map(|v| v == "true")
                    .unwrap_or(false);
                current_meta = Some((plugin_idx, meta.clone()));
                last_advance = Instant::now();
                // Reflect the newly displayed photo in the remote's status endpoint.
                if let Some(status) = &remote_status {
                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                    s.index = q_idx;
                    s.total = queue.len();
                    s.filename = meta.filename.clone();
                    s.album = meta.extra.get("album").cloned().unwrap_or_default();
                    s.favorite = favorite;
                }
            }

            // Top up again straight after the advance so a zero/short slide
            // duration (which never enters the idle branch above) still keeps
            // the buffer fed.
            self.prefetch_one(&queue, &mut cursor, &mut prefetched, prefetch_n, renderer)
                .await;
        }
    }

    /// Fetch *and decode* the next queued photo into the prefetch ring if
    /// there's room.
    ///
    /// No-op when the buffer is already at `prefetch_n` (just the length
    /// check — cheap to call every idle tick) or the queue is empty. `cursor`
    /// is a read-ahead pointer that wraps around the queue, so the ring keeps
    /// reading forward forever without ever exceeding `prefetch_n` entries.
    ///
    /// The cursor is advanced before the (slow) fetch+decode so a photo that
    /// fails to download or decode is simply dropped — it never wedges the
    /// ring, and the next tick moves on to the following photo.
    async fn prefetch_one(
        &self,
        queue: &[(usize, PhotoMeta)],
        cursor: &mut usize,
        prefetched: &mut VecDeque<(usize, PhotoMeta, RgbaImage, Option<String>)>,
        prefetch_n: usize,
        renderer: &Renderer,
    ) {
        if prefetched.len() >= prefetch_n || *cursor >= queue.len() {
            return;
        }
        let idx = *cursor;
        let (pidx, meta) = &queue[idx];
        *cursor += 1;
        if *cursor >= queue.len() {
            *cursor = 0;
        }

        let Some(bytes) = self.fetch_photo(*pidx, meta, renderer).await else {
            return;
        };
        // Decode + scale here, during the idle window, so display is instant.
        match renderer.decode_and_scale(&bytes) {
            Ok((rgba, exif_date)) => prefetched.push_back((idx, meta.clone(), rgba, exif_date)),
            Err(e) => warn!("Decode error ({}): {}", meta.filename, e),
        }
    }

    // ── Favourites ──────────────────────────────────────────────────────────

    /// Toggle the favourite state of the on-screen photo via its source plugin.
    ///
    /// On success the local metadata and the remote status are updated so the
    /// next render shows the ♥ and the phone remote reflects the change. The
    /// on-disk image already displayed is not re-rendered — the indicator
    /// appears when the photo next comes around. Plugins that don't support
    /// favourites return an error, which is logged and otherwise ignored.
    async fn toggle_favorite(
        &self,
        current_meta: &mut Option<(usize, PhotoMeta)>,
        remote_status: &Option<SharedStatus>,
    ) {
        let Some((plugin_idx, meta)) = current_meta.as_mut() else {
            debug!("Favourite toggle ignored — no photo on screen yet");
            return;
        };
        let currently = meta
            .extra
            .get("favorite")
            .map(|v| v == "true")
            .unwrap_or(false);
        let target = !currently;

        match self.plugins[*plugin_idx].set_favorite(&*meta, target).await {
            Ok(()) => {
                if target {
                    meta.extra.insert("favorite".into(), "true".into());
                } else {
                    meta.extra.remove("favorite");
                }
                info!(
                    "{} photo: {}",
                    if target {
                        "Favourited"
                    } else {
                        "Un-favourited"
                    },
                    meta.filename
                );
                if let Some(status) = remote_status {
                    status.lock().unwrap_or_else(|e| e.into_inner()).favorite = target;
                }
            }
            Err(e) => warn!("Favourite toggle failed: {e}"),
        }
    }

    // ── Settings menu ─────────────────────────────────────────────────────

    /// `(name, is_active)` for every configured `[[plugins]]` entry, in config
    /// order. Source-agnostic: lists whatever sources the config declares, so
    /// the menu works for any package/extension that is configured.
    fn source_list(&self) -> Vec<(String, bool)> {
        self.config
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.enabled))
            .collect()
    }

    /// Apply a menu action other than pause (pause is a loop local). Returns how
    /// the display loop should react. Never fails the slideshow — problems are
    /// logged and reported as `Stay`.
    async fn handle_menu_action(
        &mut self,
        action: MenuAction,
        renderer: &mut Renderer,
    ) -> MenuOutcome {
        match action {
            MenuAction::TogglePause => MenuOutcome::Stay, // handled by caller
            MenuAction::CycleTransition => {
                self.config.display.transition = next_transition(&self.config.display.transition);
                renderer.set_display_config(self.config.display.clone());
                MenuOutcome::Stay
            }
            MenuAction::CycleSlideDuration => {
                self.config.display.slide_duration_secs =
                    next_slide_secs(self.config.display.slide_duration_secs);
                MenuOutcome::Stay
            }
            MenuAction::ToggleFillScreen => {
                self.config.display.fill_screen = !self.config.display.fill_screen;
                renderer.set_display_config(self.config.display.clone());
                MenuOutcome::ReloadFrames
            }
            MenuAction::ToggleLetterboxBlur => {
                self.config.display.letterbox_blur = !self.config.display.letterbox_blur;
                renderer.set_display_config(self.config.display.clone());
                MenuOutcome::ReloadFrames
            }
            MenuAction::ToggleOsd => {
                self.config.display.show_osd = !self.config.display.show_osd;
                MenuOutcome::Stay
            }
            MenuAction::ToggleClock => {
                self.config.display.show_clock = !self.config.display.show_clock;
                MenuOutcome::Stay
            }
            MenuAction::CycleOrder => {
                self.config.display.order = next_order(&self.config.display.order);
                match Self::build_queue_with(&self.plugins, &self.config.display).await {
                    Ok(q) => MenuOutcome::NewQueue(q),
                    Err(e) => {
                        warn!("Re-order failed: {e}");
                        MenuOutcome::Stay
                    }
                }
            }
            MenuAction::SwitchSource(idx) => self.switch_source(idx, renderer).await,
            MenuAction::SaveConfig => {
                match self.save_config() {
                    Ok(()) => info!("Settings saved to {}", self.config_path.display()),
                    Err(e) => warn!("Could not save settings: {e}"),
                }
                MenuOutcome::Stay
            }
            MenuAction::Exit => MenuOutcome::Quit,
        }
    }

    /// Switch the active photo source to `config.plugins[idx]`, end to end:
    /// rebuild the plugin set with only that source enabled, initialise and
    /// authenticate it, then build a fresh queue. On *any* failure the previous
    /// source is left running untouched and the reason is logged — the
    /// slideshow never breaks because a switch didn't pan out.
    async fn switch_source(&mut self, idx: usize, renderer: &mut Renderer) -> MenuOutcome {
        if idx >= self.config.plugins.len() {
            return MenuOutcome::Stay;
        }
        // Trial config: enable only the chosen source (immutable update — the
        // live config is untouched until the switch fully succeeds).
        let mut trial = self.config.clone();
        for (i, p) in trial.plugins.iter_mut().enumerate() {
            p.enabled = i == idx;
        }
        let name = trial.plugins[idx].name.clone();

        let mut new_plugins = (self.factory)(&trial);
        if new_plugins.is_empty() {
            warn!("Cannot switch to '{name}': not compiled into this build");
            return MenuOutcome::Stay;
        }

        for plugin in &mut new_plugins {
            let pcfg = trial
                .plugin_config(plugin.name())
                .cloned()
                .unwrap_or_default();
            if let Err(e) = plugin.init(&pcfg).await {
                warn!("Cannot switch to '{name}': init failed: {e}");
                return MenuOutcome::Stay;
            }
            match plugin.authenticate().await {
                Ok(AuthStatus::Authenticated) => {}
                Ok(AuthStatus::NotAuthenticated) => {
                    warn!("Cannot switch to '{name}': not authenticated");
                    return MenuOutcome::Stay;
                }
                Ok(AuthStatus::PendingUserAction { .. }) => {
                    warn!(
                        "Cannot switch to '{name}': needs interactive setup — \
                         configure it and restart"
                    );
                    return MenuOutcome::Stay;
                }
                Err(e) => {
                    warn!("Cannot switch to '{name}': auth error: {e}");
                    return MenuOutcome::Stay;
                }
            }
        }

        match Self::build_queue_with(&new_plugins, &trial.display).await {
            Ok(queue) if !queue.is_empty() => {
                info!("Switched source to '{name}' ({} photos)", queue.len());
                self.config = trial;
                self.plugins = new_plugins;
                renderer.set_display_config(self.config.display.clone());
                MenuOutcome::Switched(queue)
            }
            Ok(_) => {
                warn!("Cannot switch to '{name}': source returned no photos");
                MenuOutcome::Stay
            }
            Err(e) => {
                warn!("Cannot switch to '{name}': listing failed: {e}");
                MenuOutcome::Stay
            }
        }
    }

    /// Persist the current in-memory config back to the config file. Writes are
    /// explicit (this menu item only) — never on every toggle — to spare the
    /// Pi's SD card. Note this rewrites the file without the template's
    /// explanatory comments: the settings survive, the prose does not.
    fn save_config(&self) -> Result<()> {
        let text = toml::to_string_pretty(&self.config).context("serialising config to TOML")?;
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.config_path, text)
            .with_context(|| format!("writing config {}", self.config_path.display()))?;
        Ok(())
    }

    // ── Fetching ──────────────────────────────────────────────────────────

    async fn fetch_photo(
        &self,
        plugin_idx: usize,
        meta: &PhotoMeta,
        renderer: &Renderer,
    ) -> Option<Vec<u8>> {
        let plugin = &self.plugins[plugin_idx];
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

/// Copy `src` into the centre of `dst`, clipped on every edge. Handles `src`
/// both smaller than `dst` (letterbox — centred with a border) and larger
/// (fill crop — centre region copied). Straight row memcpy, no blending.
fn blit_center(dst: &mut RgbaImage, src: &RgbaImage) {
    let (dw, dh) = dst.dimensions();
    let (sw, sh) = src.dimensions();
    let ox = (dw as i32 - sw as i32) / 2;
    let oy = (dh as i32 - sh as i32) / 2;
    let dstride = dw as usize * 4;
    let sstride = sw as usize * 4;
    let dbuf = dst.as_mut();
    let sbuf = src.as_raw();
    for sy in 0..sh as i32 {
        let dy = oy + sy;
        if dy < 0 || dy >= dh as i32 {
            continue;
        }
        let dx0 = ox.max(0);
        let sx0 = dx0 - ox; // ≥ 0 by construction
        let copy_w = (sw as i32 - sx0).min(dw as i32 - dx0);
        if copy_w <= 0 {
            continue;
        }
        let d = dy as usize * dstride + dx0 as usize * 4;
        let s = sy as usize * sstride + sx0 as usize * 4;
        let n = copy_w as usize * 4;
        dbuf[d..d + n].copy_from_slice(&sbuf[s..s + n]);
    }
}

// ── Menu value cyclers ───────────────────────────────────────────────────────

fn next_transition(t: &Transition) -> Transition {
    match t {
        Transition::Cut => Transition::Fade,
        Transition::Fade => Transition::SlideLeft,
        Transition::SlideLeft => Transition::SlideRight,
        Transition::SlideRight => Transition::Cut,
    }
}

fn next_order(o: &PhotoOrder) -> PhotoOrder {
    match o {
        PhotoOrder::Shuffle => PhotoOrder::Chronological,
        PhotoOrder::Chronological => PhotoOrder::NewestFirst,
        PhotoOrder::NewestFirst => PhotoOrder::DateCluster,
        PhotoOrder::DateCluster => PhotoOrder::Shuffle,
    }
}

fn next_slide_secs(current: u64) -> u64 {
    const PRESETS: [u64; 6] = [3, 5, 10, 15, 30, 60];
    match PRESETS.iter().position(|&s| s == current) {
        Some(i) => PRESETS[(i + 1) % PRESETS.len()],
        None => 10,
    }
}

// ── Fisher-Yates shuffle (no_std-safe, no rand dep) ──────────────────────────

fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut s = seed;
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let j = (s as usize) % (i + 1);
        v.swap(i, j);
    }
}

// ── Queue ordering helpers ────────────────────────────────────────────────────

/// "On this day": photos taken on today's calendar date (any year) get woven
/// near the front of the shuffled queue, one every `SPACING` slides, so
/// anniversaries surface early without taking over the rotation.
fn weave_on_this_day(v: &mut Vec<(usize, PhotoMeta)>) {
    use chrono::Datelike;
    const SPACING: usize = 8;

    let today = chrono::Local::now();
    let (month, day) = (today.month(), today.day());

    let mut on_this_day = Vec::new();
    let mut rest = Vec::with_capacity(v.len());
    for item in v.drain(..) {
        let matches_today = item
            .1
            .taken_at
            .map(|t| {
                let local = t.with_timezone(&chrono::Local);
                local.month() == month && local.day() == day
            })
            .unwrap_or(false);
        if matches_today {
            on_this_day.push(item)
        } else {
            rest.push(item)
        }
    }

    if on_this_day.is_empty() {
        *v = rest;
        return;
    }
    info!(
        "On this day: boosting {} photo(s) taken on this date",
        on_this_day.len()
    );

    let boosted_count = on_this_day.len();
    let mut boosted = on_this_day.into_iter();
    let mut out = Vec::with_capacity(rest.len() + boosted_count);
    for (i, item) in rest.into_iter().enumerate() {
        if i % SPACING == 0 {
            if let Some(b) = boosted.next() {
                out.push(b);
            }
        }
        out.push(item);
    }
    out.extend(boosted); // more boosted photos than slots — append the rest
    *v = out;
}

/// Date-cluster ordering: group photos by capture date (falling back to
/// album), keep each group chronological, split groups into runs of at most
/// `MAX_CLUSTER`, then shuffle the runs. The slideshow tells small "stories"
/// instead of jumping randomly between decades.
fn date_cluster_order(all: Vec<(usize, PhotoMeta)>, seed: u64) -> Vec<(usize, PhotoMeta)> {
    use std::collections::HashMap;
    const MAX_CLUSTER: usize = 5;

    let mut groups: HashMap<String, Vec<(usize, PhotoMeta)>> = HashMap::new();
    for item in all {
        let key = item
            .1
            .taken_at
            .map(|t| t.format("%Y-%m-%d").to_string())
            .or_else(|| item.1.extra.get("album").cloned())
            .unwrap_or_default();
        groups.entry(key).or_default().push(item);
    }

    // Deterministic group walk before the seeded shuffle.
    let mut keys: Vec<String> = groups.keys().cloned().collect();
    keys.sort();

    let mut clusters: Vec<Vec<(usize, PhotoMeta)>> = Vec::new();
    for key in keys {
        // Keys came from `groups` itself, so remove always succeeds — but
        // skip rather than panic if that invariant ever breaks.
        if let Some(mut group) = groups.remove(&key) {
            group.sort_by_key(|(_, m)| m.taken_at);
            for chunk in group.chunks(MAX_CLUSTER) {
                clusters.push(chunk.to_vec());
            }
        }
    }

    shuffle(&mut clusters, seed);
    clusters.into_iter().flatten().collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_cycles_through_all_four_and_wraps() {
        let mut t = Transition::Cut;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(format!("{t:?}"));
            t = next_transition(&t);
        }
        // Visited all four, and wrapped back to the start.
        assert_eq!(seen.len(), 4);
        assert!(matches!(t, Transition::Cut));
    }

    #[test]
    fn order_cycles_and_wraps() {
        let mut o = PhotoOrder::Shuffle;
        for _ in 0..4 {
            o = next_order(&o);
        }
        assert!(matches!(o, PhotoOrder::Shuffle));
    }

    #[test]
    fn slide_secs_cycle_and_snap_unknown_to_ten() {
        assert_eq!(next_slide_secs(3), 5);
        assert_eq!(next_slide_secs(60), 3); // wraps
        assert_eq!(next_slide_secs(7), 10); // not a preset → snaps to 10
    }

    #[test]
    fn blit_center_centres_smaller_source_without_overflow() {
        let mut dst = RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 255]));
        let src = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
        blit_center(&mut dst, &src);
        // Centre pixel comes from src (red); a corner stays black.
        assert_eq!(dst.get_pixel(5, 5)[0], 255);
        assert_eq!(dst.get_pixel(0, 0)[0], 0);
    }

    #[test]
    fn blit_center_crops_larger_source_without_panicking() {
        let mut dst = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]));
        let src = RgbaImage::from_pixel(10, 10, Rgba([0, 255, 0, 255]));
        blit_center(&mut dst, &src); // must not panic; fills with src
        assert_eq!(dst.get_pixel(2, 2)[1], 255);
    }
}
