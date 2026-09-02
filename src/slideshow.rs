//! Slideshow engine.
//!
//! Runs on the Tokio runtime. A background task pre-fetches the next N images
//! while the current one is on screen, so transitions are instant on slow Pi
//! Zero I/O. Plugin fetch and decode run on `Fetcher` tasks so the display
//! loop never awaits I/O.
use crate::cache::CacheHandle;
use crate::compose::blit_center;
use crate::config::{Config, DisplayConfig, PhotoOrder, Transition};
use crate::fetcher::{
    stall_state, FetchDone, FetchJob, Fetcher, JobKind, StallAction, MAX_IN_FLIGHT_FETCHES,
};
use crate::fullscreen_controller::FullscreenController;
use crate::gallery_controller::GalleryController;
use crate::menu::{EditField, Menu, MenuAction};
use crate::mode::Mode;
use crate::plugin::{AuthStatus, BoxedPlugin, PhotoMeta, PhotoPlugin};
use crate::queue_io::{
    self, filter_unseen_photos, ExtendResult, FavoriteResult, QueueIo, QueueLoader,
    EXTEND_MAX_ROUNDS, MAX_PHOTOS_PER_PLUGIN, PAGE_SIZE,
};
use crate::remote::SharedStatus;
use crate::renderer::{DisplayEnv, Renderer, SlideshowCmd};
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use log::{debug, info, warn};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;

/// Thumbnails spawned per gallery tick — balances Pi Zero CPU with fast fill.
const GALLERY_THUMBS_PER_TICK: usize = 4;
/// Exclusive `authenticate()` call budget. Pending-user-action waits use
/// `[auth].pending_timeout_secs` instead.
const AUTH_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Settings-menu title. Used both to render the panel and to compute its
/// geometry for click/hover hit-testing, so the two must use the same string.
const MENU_TITLE: &str = "PicoGallery - Settings";
const MAX_PREFETCH_ATTEMPTS_PER_TICK: usize = 8;

/// Shared plugin handle. Fetch runs on `&self` so the display loop and
/// background `Fetcher` tasks can hold clones without exclusive access.
pub type SharedPlugin = Arc<dyn PhotoPlugin>;
/// Builds fresh plugin instances from a config. Lets the engine rebuild its
/// photo sources at runtime (e.g. when the user switches source from the
/// menu) without the slideshow needing to know which plugins were compiled in
/// — the menu therefore works for any package/extension.
pub type PluginFactory = Box<dyn Fn(&Config) -> Vec<BoxedPlugin>>;

pub struct Slideshow {
    config: Config,
    plugins: Vec<SharedPlugin>,
    cache: CacheHandle,
    /// Where to persist settings when the user picks "Save settings".
    config_path: PathBuf,
    /// Rebuilds the plugin set from a config — used to switch source at runtime.
    factory: PluginFactory,
    display_env: DisplayEnv,
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
    /// Clear the no-repeat-shown memory without rebuilding the queue.
    ResetShown,
}

/// Drive `fut` while pumping SDL so a plugin/network await cannot freeze the
/// KMS/DRM display (and so Quit still reaches the loop). `Err` is only Quit.
async fn pump_sdl_until<T>(
    renderer: &mut Renderer,
    fut: impl std::future::Future<Output = T>,
) -> std::result::Result<T, MenuOutcome> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            result = &mut fut => return Ok(result),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {
                let cmds = renderer.poll_events(true, false, false, false);
                if cmds.iter().any(|c| matches!(c, SlideshowCmd::Quit)) {
                    return Err(MenuOutcome::Quit);
                }
            }
        }
    }
}

impl Slideshow {
    pub async fn new(
        config: Config,
        plugins: Vec<BoxedPlugin>,
        config_path: PathBuf,
        factory: PluginFactory,
        display_env: DisplayEnv,
    ) -> Result<Self> {
        let cache =
            CacheHandle::open_or_degrade(&config.cache.resolved_dir(), config.cache.max_mb).await;
        Ok(Self {
            config,
            plugins: share_plugins(plugins),
            cache,
            config_path,
            factory,
            display_env,
        })
    }

    /// Run the slideshow.  Blocks the calling thread until the user quits.
    ///
    /// `remote_rx` / `remote_status` come from `remote::start` when the HTTP
    /// remote is enabled; both are `None` otherwise.
    pub async fn run(
        mut self,
        remote_rx: Option<Receiver<SlideshowCmd>>,
        cec_rx: Option<Receiver<SlideshowCmd>>,
        remote_status: Option<SharedStatus>,
        shutdown_rx: Receiver<()>,
    ) -> Result<()> {
        // 1. Renderer first so pending-auth OSD can paint; then authenticate.
        let mut renderer = Renderer::init(self.config.display.clone(), self.display_env.clone())?;
        self.authenticate_all(&mut renderer).await?;

        // 2. Build the initial play queue. A source may be temporarily offline
        // after authentication, so give retryable providers a short bounded
        // window before declaring the appliance empty.
        let mut queue = Vec::new();
        for attempt in 0..3 {
            queue = self.build_queue().await?;
            if !queue.is_empty() {
                break;
            }
            let delay = Duration::from_secs(1 << attempt);
            warn!(
                "No photos available yet; retrying providers in {}s",
                delay.as_secs()
            );
            tokio::time::sleep(delay).await;
        }
        if queue.is_empty() {
            anyhow::bail!(
                "No photos found across all plugins. Check your config and photo source \
                 (PhotoPrism URL/credentials, or add images to the directory plugin path). \
                 Run: journalctl -u picogallery -n 50"
            );
        }
        info!("Play queue: {} photos", queue.len());
        if let Some(status) = &remote_status {
            let mut snapshot = status.lock().await;
            snapshot.total = queue.len();
            snapshot.providers = self.plugins.len();
        }

        // 3. Main display loop.
        let result = self
            .display_loop(
                &mut renderer,
                queue,
                remote_rx,
                cec_rx,
                remote_status,
                shutdown_rx,
            )
            .await;
        shutdown_shared(&self.plugins).await;
        result
    }

    // ── Authentication ────────────────────────────────────────────────────

    async fn authenticate_all(&mut self, renderer: &mut Renderer) -> Result<()> {
        let pending_timeout = Duration::from_secs(self.config.auth.pending_timeout_secs);
        let plugins = std::mem::take(&mut self.plugins);
        let kept = authenticate_plugin_set(
            plugins,
            AUTH_CALL_TIMEOUT,
            pending_timeout,
            |name, message, remaining| {
                draw_sign_in_osd(renderer, name, message, remaining);
                renderer
                    .poll_events(false, false, false, false)
                    .into_iter()
                    .any(|c| matches!(c, SlideshowCmd::Quit))
            },
        )
        .await?;
        self.plugins = kept;
        Ok(())
    }

    // ── Queue building ────────────────────────────────────────────────────

    async fn build_queue(&self) -> Result<Vec<(usize, PhotoMeta)>> {
        Self::build_queue_with(&self.plugins, &self.config.display, false).await
    }

    /// Build the play queue from an explicit plugin set and display config.
    /// `full = false` loads one API page per plugin (fast gallery startup);
    /// `full = true` pages until each plugin is exhausted (menu re-order / switch).
    async fn build_queue_with(
        plugins: &[SharedPlugin],
        display: &DisplayConfig,
        full: bool,
    ) -> Result<Vec<(usize, PhotoMeta)>> {
        let mut loader = QueueLoader::new(plugins.len());
        let mut all: Vec<(usize, PhotoMeta)> = Vec::new();
        loop {
            let batch = Self::fetch_queue_round(plugins, &mut loader).await;
            if batch.is_empty() {
                break;
            }
            all.extend(batch);
            if !full {
                break;
            }
        }

        apply_order(&mut all, display, loader.shuffle_seed);

        match display.order {
            PhotoOrder::Shuffle => info!("Photo order: shuffle ({} photos)", all.len()),
            PhotoOrder::Chronological => info!("Photo order: chronological ({} photos)", all.len()),
            PhotoOrder::NewestFirst => info!("Photo order: newest first ({} photos)", all.len()),
            PhotoOrder::DateCluster => info!("Photo order: date clusters ({} photos)", all.len()),
        }

        Ok(all)
    }

    /// Pull one API page from every plugin that still has more photos.
    async fn fetch_queue_round(
        plugins: &[SharedPlugin],
        loader: &mut QueueLoader,
    ) -> Vec<(usize, PhotoMeta)> {
        queue_io::fetch_queue_round(plugins, loader, PAGE_SIZE, MAX_PHOTOS_PER_PLUGIN).await
    }

    /// Sync: spawn background paging when near the end (or forced).
    fn request_extend(
        &self,
        queue_io: &mut QueueIo,
        queue_loader: &QueueLoader,
        queue_ids: &HashSet<(usize, String)>,
        force: bool,
        trigger_idx: usize,
        queue_len: usize,
    ) {
        if queue_io.extend_in_flight() {
            return;
        }
        let should = if force {
            !queue_loader.all_exhausted()
        } else {
            queue_loader.near_end(trigger_idx, queue_len)
        };
        if !should {
            return;
        }
        let _ = queue_io.try_spawn_extend(
            self.plugins.clone(),
            queue_loader.clone(),
            queue_ids.clone(),
            EXTEND_MAX_ROUNDS,
            PAGE_SIZE,
            MAX_PHOTOS_PER_PLUGIN,
        );
    }

    /// Merge a completed extend into the live queue (display-loop only).
    async fn apply_extend_result(
        &self,
        done: ExtendResult,
        queue: &mut Vec<(usize, PhotoMeta)>,
        queue_ids: &mut HashSet<(usize, String)>,
        queue_loader: &mut QueueLoader,
        remote_status: &Option<SharedStatus>,
    ) -> bool {
        *queue_loader = done.loader;
        if done.photos.is_empty() {
            return false;
        }
        let before = queue.len();
        let fresh = filter_unseen_photos(done.photos, queue_ids);
        if fresh.is_empty() {
            return false;
        }
        let from = queue.len();
        queue.extend(fresh);
        apply_order_tail(queue, from, &self.config.display, queue_loader.shuffle_seed);
        info!(
            "Loaded {} more photos ({} total)",
            queue.len() - before,
            queue.len()
        );
        if let Some(status) = remote_status {
            status.lock().await.total = queue.len();
        }
        true
    }

    /// Apply favourite result; revert optimistic flip on failure.
    async fn apply_favorite_result(
        fav: FavoriteResult,
        current_meta: &mut Option<(usize, PhotoMeta)>,
        remote_status: &Option<SharedStatus>,
    ) {
        let Some((plugin_idx, meta)) = current_meta.as_mut() else {
            return;
        };
        if *plugin_idx != fav.plugin_idx || meta.id != fav.photo_id {
            return;
        }
        if fav.ok {
            meta.is_favorite = fav.favorite;
            if let Some(status) = remote_status {
                status.lock().await.favorite = fav.favorite;
            }
            info!(
                "{} photo: {}",
                if fav.favorite {
                    "Favourited"
                } else {
                    "Un-favourited"
                },
                meta.filename
            );
        } else {
            meta.is_favorite = !fav.favorite;
            if let Some(status) = remote_status {
                status.lock().await.favorite = meta.is_favorite;
            }
            if let Some(err) = &fav.error {
                warn!("Favourite toggle failed: {err}");
            } else {
                warn!("Favourite toggle failed");
            }
        }
    }

    /// Optimistic favourite flip + background `set_favorite` (plugin I/O not awaited).
    async fn request_favorite_toggle(
        &self,
        queue_io: &mut QueueIo,
        current_meta: &mut Option<(usize, PhotoMeta)>,
        remote_status: &Option<SharedStatus>,
    ) {
        let Some((plugin_idx, meta)) = current_meta.as_mut() else {
            debug!("Favourite toggle ignored — no photo on screen yet");
            return;
        };
        if !self.plugins[*plugin_idx].capabilities().favorite_toggle {
            debug!(
                "Favourite toggle ignored — source '{}' has no favourite support",
                self.plugins[*plugin_idx].name()
            );
            return;
        }
        if queue_io.fav_in_flight() {
            return;
        }
        let target = !meta.is_favorite;
        meta.is_favorite = target;
        if let Some(status) = remote_status {
            status.lock().await.favorite = target;
        }
        let plugin = self.plugins[*plugin_idx].clone();
        let meta_clone = meta.clone();
        let _ = queue_io.try_spawn_favorite(plugin, *plugin_idx, meta_clone, target);
    }

    // ── Display loop ──────────────────────────────────────────────────────

    async fn display_loop(
        &mut self,
        renderer: &mut Renderer,
        mut queue: Vec<(usize, PhotoMeta)>,
        mut remote_rx: Option<Receiver<SlideshowCmd>>,
        mut cec_rx: Option<Receiver<SlideshowCmd>>,
        remote_status: Option<SharedStatus>,
        mut shutdown_rx: Receiver<()>,
    ) -> Result<()> {
        let tc = renderer.texture_creator();
        let mut queue_loader = QueueLoader::new(self.plugins.len());
        queue_loader.sync_counts(&queue);
        let mut queue_ids: HashSet<(usize, String)> =
            queue.iter().map(|(pi, m)| (*pi, m.id.clone())).collect();

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

        let gallery_mode = self.config.display.gallery_mode;
        let mut mode = if gallery_mode {
            Mode::Gallery
        } else {
            Mode::Fullscreen
        };
        let mut gallery_ctl = GalleryController::new(renderer.width());
        let mut fullscreen_ctl = FullscreenController::new();
        if mode.is_gallery() {
            gallery_ctl.enter();
        }
        let mut gallery_thumb_cursor = 0usize;

        let no_repeat_shown = self.config.display.no_repeat_shown;
        let mut shown_ids: Vec<HashSet<String>> =
            (0..self.plugins.len()).map(|_| HashSet::new()).collect();
        let mut shown_count = 0usize;
        let mut menu_frame = RgbaImage::from_pixel(
            renderer.width().max(1),
            renderer.height().max(1),
            Rgba([0, 0, 0, 255]),
        );
        // First frame after opening from the grid uses Cut (no fade from grid).
        let mut open_cut_once = false;

        let mut fetcher = Fetcher::new(
            self.plugins.clone(),
            self.cache.clone(),
            renderer.image_processor(),
            MAX_IN_FLIGHT_FETCHES,
        );
        let mut queue_io = QueueIo::new();
        let mut stall_since: Option<Instant> = None;
        let mut stall_warned = false;
        let mut stall_osd = false;

        // Pre-warm: spawn only — never await plugin I/O before poll_events.
        let mut cursor = 0usize;
        if mode.is_fullscreen() {
            Self::pump_slide_fetches(
                &mut fetcher,
                &queue,
                &mut cursor,
                &prefetched,
                prefetch_n,
                no_repeat_shown,
                &shown_ids,
            );
        }

        let slide_dur = Duration::from_secs(self.config.display.slide_duration_secs);

        // Initialize last_advance so that the first photo shows immediately
        let mut last_advance = Instant::now()
            .checked_sub(slide_dur)
            .unwrap_or_else(Instant::now);

        // Thermal guard: sample CPU temp at most once every few seconds instead
        // of every loop iteration (~20×/s), so the cooling feature doesn't itself
        // add steady sysfs I/O churn on a Pi Zero. `None` until the first sample.
        const TEMP_POLL_INTERVAL: Duration = Duration::from_secs(5);
        let mut cached_temp: Option<f32> = None;
        let mut last_temp_check: Option<Instant> = None;

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
            let mut cmds: Vec<SlideshowCmd> = renderer.poll_events(
                menu.open,
                menu.editing.is_some(),
                gallery_mode,
                mode.is_gallery(),
            );
            if let Some(rx) = remote_rx.as_mut() {
                while let Ok(cmd) = rx.try_recv() {
                    cmds.push(cmd);
                }
            }
            if let Some(rx) = cec_rx.as_mut() {
                while let Ok(cmd) = rx.try_recv() {
                    cmds.push(cmd);
                }
            }
            if shutdown_rx.try_recv().is_ok() {
                info!("OS shutdown signal received.");
                cmds.push(SlideshowCmd::Quit);
            }

            // Menu rows + labels for *this* input batch, built once so clicks
            // map to the exact layout poll_events hit-tested against. Only built
            // when the menu is open *and* there is input to interpret — an
            // idle-open menu allocates nothing each tick (Pi Zero stays cold).
            let action_rows = if menu.open && !cmds.is_empty() {
                let sources = self.source_list();
                self.build_menu_rows(paused, &menu, &sources)
            } else {
                Vec::new()
            };
            // Flattened view for hit-testing (borrows the rows just built).
            let action_items: Vec<crate::osd::MenuItem> = action_rows
                .iter()
                .map(|r| crate::osd::MenuItem {
                    label: &r.label,
                    is_header: matches!(r.kind, crate::menu::RowKind::Header),
                })
                .collect();
            // At most one activation per batch — users don't double-click inside
            // a 30 ms tick, and this keeps the borrow-heavy action handling out
            // of the per-command match.
            let mut pending_activate = false;
            // Deferred so we can await the selected-photo fetch outside the match.
            // pending open lives on fullscreen_ctl

            for cmd in cmds {
                match cmd {
                    SlideshowCmd::Quit => {
                        info!("Quit requested.");
                        self.cache.flush().await;
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
                            if mode.is_gallery() {
                                // Grid repaints on the next tick to cover the menu.
                                gallery_ctl.dirty = true;
                            } else if let Some(img) = &current_rgba {
                                // Repaint the photo underneath so the menu vanishes.
                                let _ = renderer.show_cut(img, &tc);
                            }
                            if !paused {
                                last_advance = Instant::now();
                            }
                        }
                    }
                    SlideshowCmd::MenuMove(d) => {
                        if menu.open && !action_rows.is_empty() {
                            // Step to the next selectable row, skipping headers.
                            menu.selected =
                                crate::menu::next_selectable(&action_rows, menu.selected, d);
                            menu_dirty = true;
                        }
                    }
                    SlideshowCmd::MenuPoint { x, y } => {
                        if menu.open {
                            if let Some(idx) = crate::osd::menu_hit_test(
                                renderer.width(),
                                renderer.height(),
                                MENU_TITLE,
                                &action_items,
                                x,
                                y,
                            ) {
                                // Don't move the highlight onto a section header.
                                let is_item = action_rows
                                    .get(idx)
                                    .map(|r| r.kind == crate::menu::RowKind::Item)
                                    .unwrap_or(false);
                                if is_item && idx != menu.selected {
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
                                &action_items,
                                x,
                                y,
                            ) {
                                // A click on a header is inert (panel stays open);
                                // a click on an item selects and activates it.
                                Some(idx)
                                    if action_rows
                                        .get(idx)
                                        .map(|r| r.kind == crate::menu::RowKind::Item)
                                        .unwrap_or(false) =>
                                {
                                    menu.selected = idx;
                                    pending_activate = true;
                                }
                                Some(_) => {}
                                // Click outside the panel dismisses the menu.
                                None => {
                                    menu.open = false;
                                    if mode.is_gallery() {
                                        gallery_ctl.dirty = true;
                                    } else if let Some(img) = &current_rgba {
                                        let _ = renderer.show_cut(img, &tc);
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
                        if !menu.open && mode.is_fullscreen() {
                            paused = !paused;
                            info!("Slideshow {}.", if paused { "paused" } else { "resumed" });
                            last_advance = Instant::now();
                            if let Some(status) = &remote_status {
                                status.lock().await.paused = paused;
                            }
                        }
                    }
                    SlideshowCmd::Next => {
                        if !menu.open && mode.is_fullscreen() {
                            if current_queue_idx + 1 >= queue.len() {
                                self.request_extend(
                                    &mut queue_io,
                                    &queue_loader,
                                    &queue_ids,
                                    true,
                                    current_queue_idx,
                                    queue.len(),
                                );
                            }
                            last_advance = Instant::now()
                                .checked_sub(slide_dur)
                                .unwrap_or_else(Instant::now); // force advance
                        }
                    }
                    SlideshowCmd::Prev => {
                        if !menu.open && mode.is_fullscreen() {
                            current_queue_idx = if current_queue_idx == 0 {
                                queue.len().saturating_sub(1)
                            } else {
                                current_queue_idx - 1
                            };
                            prefetched.clear();
                            fetcher.invalidate();
                            cursor = current_queue_idx;
                            last_advance = Instant::now()
                                .checked_sub(slide_dur)
                                .unwrap_or_else(Instant::now);
                        }
                    }
                    SlideshowCmd::ToggleFavorite => {
                        if !menu.open && mode.is_fullscreen() {
                            self.request_favorite_toggle(
                                &mut queue_io,
                                &mut current_meta,
                                &remote_status,
                            )
                            .await;
                        }
                    }
                    // ── Text field editing (only meaningful while a field is open) ──
                    SlideshowCmd::TextChar(c) => {
                        if menu.editing.is_some() {
                            // Cap length and drop control chars so a stray key
                            // can't bloat or corrupt the field.
                            const MAX_FIELD_CHARS: usize = 128;
                            if !c.is_control() && menu.buffer.chars().count() < MAX_FIELD_CHARS {
                                menu.buffer.push(c);
                                menu_dirty = true;
                            }
                        }
                    }
                    SlideshowCmd::TextBackspace => {
                        if menu.editing.is_some() {
                            menu.buffer.pop();
                            menu_dirty = true;
                        }
                    }
                    SlideshowCmd::TextCommit => {
                        if let Some(field) = menu.editing.take() {
                            let value = std::mem::take(&mut menu.buffer);
                            self.commit_edit(field, value);
                            menu_dirty = true;
                        }
                    }
                    SlideshowCmd::TextCancel => {
                        if menu.editing.is_some() {
                            menu.editing = None;
                            menu.buffer.clear();
                            menu_dirty = true;
                        }
                    }
                    SlideshowCmd::BackToGallery => {
                        if gallery_mode && mode.is_fullscreen() {
                            mode = Mode::Gallery;
                            gallery_ctl.enter();
                            gallery_ctl.mark_dirty();
                            paused = false;
                            prefetched.clear();
                            fetcher.invalidate();
                            gallery_ctl.grid.clear_all_pending_thumbs();
                            current_rgba = None;
                            current_meta = None;
                            gallery_thumb_cursor = 0;
                            if gallery_mode {
                                let grid = &mut gallery_ctl.grid;
                                grid.set_selected(current_queue_idx, queue.len());
                                grid.ensure_selected_visible(renderer.height(), queue.len());
                            }
                            gallery_ctl.dirty = true;
                            info!("Returned to gallery grid.");
                        }
                    }
                    SlideshowCmd::OpenSlideshow(idx) => {
                        if gallery_mode && idx < queue.len() {
                            fullscreen_ctl.pending_open = Some(idx);
                        }
                    }
                    SlideshowCmd::GalleryClick { x, y } if gallery_mode && mode.is_gallery() => {
                        let grid = &mut gallery_ctl.grid;
                        if let Some(idx) = grid.index_at(x, y, queue.len()) {
                            grid.set_selected(idx, queue.len());
                            fullscreen_ctl.pending_open = Some(idx);
                        }
                    }
                    SlideshowCmd::GalleryClick { .. } => {}
                    SlideshowCmd::GalleryPage(dir) if gallery_mode && mode.is_gallery() => {
                        let grid = &mut gallery_ctl.grid;
                        let scrolled = grid.scroll_page(dir, queue.len(), renderer.height());
                        if !scrolled
                            && dir > 0
                            && grid.at_scroll_bottom(queue.len(), renderer.height())
                        {
                            self.request_extend(
                                &mut queue_io,
                                &queue_loader,
                                &queue_ids,
                                true,
                                grid.selected,
                                queue.len(),
                            );
                        }
                        if scrolled {
                            gallery_ctl.dirty = true;
                        }
                    }
                    SlideshowCmd::GalleryPage(_) => {}
                    SlideshowCmd::GalleryMoveSelection { dx, dy }
                        if gallery_mode && mode.is_gallery() =>
                    {
                        let grid = &mut gallery_ctl.grid;
                        let blocked = grid.move_selection(dx, dy, queue.len());
                        if blocked {
                            self.request_extend(
                                &mut queue_io,
                                &queue_loader,
                                &queue_ids,
                                true,
                                grid.selected,
                                queue.len(),
                            );
                        }
                        grid.ensure_selected_visible(renderer.height(), queue.len());
                        gallery_ctl.dirty = true;
                    }
                    SlideshowCmd::GalleryMoveSelection { .. } => {}
                    SlideshowCmd::GalleryScroll(delta) if gallery_mode && mode.is_gallery() => {
                        let grid = &mut gallery_ctl.grid;
                        let before = grid.scroll_y;
                        grid.scroll_by(delta, queue.len(), renderer.height());
                        if grid.scroll_y != before {
                            gallery_ctl.dirty = true;
                        }
                    }
                    SlideshowCmd::GalleryScroll(_) => {}
                    SlideshowCmd::GalleryOpenSelected
                        if gallery_mode && mode.is_gallery() && !queue.is_empty() =>
                    {
                        let idx = gallery_ctl.grid.selected.min(queue.len() - 1);
                        fullscreen_ctl.pending_open = Some(idx);
                    }
                    SlideshowCmd::GalleryOpenSelected => {}
                    SlideshowCmd::GalleryOpenVisible
                        if gallery_mode && mode.is_gallery() && !queue.is_empty() =>
                    {
                        let idx = gallery_ctl.grid.selected.min(queue.len() - 1);
                        fullscreen_ctl.pending_open = Some(idx);
                    }
                    SlideshowCmd::GalleryOpenVisible => {}
                }
            }

            // Spawn a priority fetch for the clicked index. Mode switch happens
            // when the result arrives — never await JPEG I/O here.
            if let Some(idx) = fullscreen_ctl.pending_open.take() {
                if idx < queue.len() {
                    prefetched.clear();
                    current_rgba = None;
                    fetcher.invalidate();
                    gallery_ctl.grid.clear_all_pending_thumbs();
                    let (pidx, meta) = &queue[idx];
                    if fetcher.try_spawn(FetchJob::Slide {
                        queue_idx: idx,
                        plugin_idx: *pidx,
                        meta: meta.clone(),
                        priority: true,
                    }) {
                        fullscreen_ctl.awaiting_open = Some(idx);
                    } else {
                        fullscreen_ctl.pending_open = Some(idx);
                    }
                }
            }

            for done in fetcher.drain() {
                self.apply_fetch_result(
                    done,
                    &mut prefetched,
                    &mut gallery_ctl,
                    &mut fullscreen_ctl,
                    &mut mode,
                    &mut current_queue_idx,
                    &mut cursor,
                    &mut open_cut_once,
                    &mut last_advance,
                    &mut paused,
                    slide_dur,
                    &queue,
                );
            }

            if let Some(done) = queue_io.drain_extend() {
                if self
                    .apply_extend_result(
                        done,
                        &mut queue,
                        &mut queue_ids,
                        &mut queue_loader,
                        &remote_status,
                    )
                    .await
                    && mode.is_gallery()
                {
                    gallery_ctl.dirty = true;
                }
            }
            if let Some(fav) = queue_io.drain_favorite() {
                Self::apply_favorite_result(fav, &mut current_meta, &remote_status).await;
            }

            if mode.is_fullscreen() {
                if prefetched.is_empty() && fetcher.in_flight() == 0 {
                    stall_since.get_or_insert(Instant::now());
                } else if !prefetched.is_empty() {
                    stall_since = None;
                    stall_warned = false;
                    stall_osd = false;
                }
                match stall_state(
                    prefetched.is_empty(),
                    fetcher.in_flight(),
                    mode.is_fullscreen(),
                    stall_since.map(|t| t.elapsed()),
                    stall_warned,
                    stall_osd,
                ) {
                    StallAction::Warn => {
                        warn!(
                            "Prefetch stalled: no photo could be fetched or decoded from {} sources — retrying",
                            self.plugins.len()
                        );
                        stall_warned = true;
                    }
                    StallAction::Osd => {
                        draw_stall_osd(renderer, &tc);
                        stall_osd = true;
                        self.request_extend(
                            &mut queue_io,
                            &queue_loader,
                            &queue_ids,
                            false,
                            current_queue_idx,
                            queue.len(),
                        );
                        Self::pump_slide_fetches(
                            &mut fetcher,
                            &queue,
                            &mut cursor,
                            &prefetched,
                            prefetch_n,
                            no_repeat_shown,
                            &shown_ids,
                        );
                    }
                    StallAction::None => {}
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
                            status.lock().await.paused = paused;
                        }
                        menu_dirty = true;
                    }
                    // Entering a text field needs the loop-local `menu`, so it is
                    // handled here rather than in the action helper. Secrets start
                    // empty; other fields preload their current value to edit.
                    MenuAction::BeginEdit(field) => {
                        menu.buffer = self.edit_initial(field);
                        menu.editing = Some(field);
                        menu_dirty = true;
                    }
                    other => match self.handle_menu_action(other, renderer).await {
                        MenuOutcome::Stay => menu_dirty = true,
                        MenuOutcome::Quit => {
                            self.cache.flush().await;
                            return Ok(());
                        }
                        MenuOutcome::ReloadFrames => {
                            prefetched.clear();
                            fetcher.invalidate();
                            queue_io.invalidate();
                            fetcher.set_processor(renderer.image_processor());
                            if !queue.is_empty() {
                                cursor = (current_queue_idx + 1) % queue.len();
                            }
                            if gallery_mode {
                                let grid = &mut gallery_ctl.grid;
                                grid.clear();
                            }
                            gallery_ctl.dirty = true;
                            menu_dirty = true;
                        }
                        MenuOutcome::NewQueue(q) => {
                            queue = q;
                            queue_loader.sync_counts(&queue);
                            queue_loader.mark_fully_loaded();
                            cursor = 0;
                            current_queue_idx = 0;
                            prefetched.clear();
                            fetcher.invalidate();
                            queue_io.invalidate();
                            shown_ids.iter_mut().for_each(HashSet::clear);
                            shown_count = 0;
                            if gallery_mode {
                                let grid = &mut gallery_ctl.grid;
                                grid.clear();
                            }
                            gallery_ctl.dirty = true;
                            menu_dirty = true;
                        }
                        MenuOutcome::Switched(q) => {
                            queue = q;
                            queue_loader.sync_counts(&queue);
                            queue_loader.mark_fully_loaded();
                            cursor = 0;
                            current_queue_idx = 0;
                            prefetched.clear();
                            fetcher.set_plugins(self.plugins.clone());
                            fetcher.set_processor(renderer.image_processor());
                            queue_io.invalidate();
                            shown_ids = (0..self.plugins.len()).map(|_| HashSet::new()).collect();
                            shown_count = 0;
                            current_meta = None;
                            current_rgba = None;
                            menu.open = false;
                            if gallery_mode {
                                let grid = &mut gallery_ctl.grid;
                                grid.clear();
                            }
                            gallery_ctl.dirty = true;
                            if mode.is_gallery() {
                                gallery_thumb_cursor = 0;
                            } else {
                                last_advance = Instant::now()
                                    .checked_sub(slide_dur)
                                    .unwrap_or_else(Instant::now);
                            }
                        }
                        MenuOutcome::ResetShown => {
                            shown_ids.iter_mut().for_each(HashSet::clear);
                            shown_count = 0;
                            menu_dirty = true;
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
                    let rows = self.build_menu_rows(paused, &menu, &sources);
                    // Keep the highlight off section headers after any row-set
                    // change (menu just opened, or a conditional group appeared).
                    menu.selected = crate::menu::snap_to_selectable(&rows, menu.selected);
                    let items: Vec<crate::osd::MenuItem> = rows
                        .iter()
                        .map(|r| crate::osd::MenuItem {
                            label: &r.label,
                            is_header: matches!(r.kind, crate::menu::RowKind::Header),
                        })
                        .collect();
                    // Composite onto a full-screen base so the menu geometry is
                    // in screen coordinates. Otherwise click/hover hit-testing
                    // (which uses the screen height) would be vertically offset
                    // whenever the photo doesn't fill the screen — e.g. plain
                    // letterbox (`letterbox_blur = false`) or `fill_screen` crop.
                    menu_frame
                        .pixels_mut()
                        .for_each(|pixel| *pixel = Rgba([0, 0, 0, 255]));
                    if let Some(img) = &current_rgba {
                        blit_center(&mut menu_frame, img);
                    }
                    crate::osd::draw_menu(&mut menu_frame, MENU_TITLE, &items, menu.selected);
                    if let Err(e) = renderer.show_cut(&menu_frame, &tc) {
                        warn!("menu render error: {e}");
                    }
                    menu_dirty = false;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                continue;
            }

            // ── Gallery grid ───────────────────────────────────────────────
            if mode.is_gallery() {
                if gallery_mode {
                    let grid = &mut gallery_ctl.grid;
                    grid.clamp_scroll(queue.len(), renderer.height());
                    let sel = grid.selected.min(queue.len().saturating_sub(1));
                    self.request_extend(
                        &mut queue_io,
                        &queue_loader,
                        &queue_ids,
                        false,
                        sel,
                        queue.len(),
                    );
                    // Spawn gallery thumbs (no await). Skip while a fullscreen
                    // open is in flight so the priority slide keeps a slot.
                    let mut spawned = 0usize;
                    if fullscreen_ctl.awaiting_open.is_none()
                        && !queue.is_empty()
                        && grid.needs_thumb(sel)
                        && fetcher.has_capacity()
                    {
                        let (pidx, meta) = &queue[sel];
                        if fetcher.try_spawn(FetchJob::Thumb {
                            queue_idx: sel,
                            plugin_idx: *pidx,
                            meta: meta.clone(),
                            cell_px: grid.cell,
                        }) {
                            grid.mark_thumb_pending(sel);
                            spawned += 1;
                        }
                    }
                    let visible = grid.visible_indices(renderer.height(), queue.len());
                    if fullscreen_ctl.awaiting_open.is_none() && !visible.is_empty() {
                        let start = gallery_thumb_cursor % visible.len();
                        for offset in 0..visible.len() {
                            if spawned >= GALLERY_THUMBS_PER_TICK || !fetcher.has_capacity() {
                                break;
                            }
                            let pick = visible[(start + offset) % visible.len()];
                            if !grid.needs_thumb(pick) {
                                continue;
                            }
                            gallery_thumb_cursor = (start + offset + 1) % visible.len();
                            let (pidx, meta) = &queue[pick];
                            if fetcher.try_spawn(FetchJob::Thumb {
                                queue_idx: pick,
                                plugin_idx: *pidx,
                                meta: meta.clone(),
                                cell_px: grid.cell,
                            }) {
                                grid.mark_thumb_pending(pick);
                                spawned += 1;
                            }
                        }
                    }
                    // Only repaint when something changed — an idle, fully
                    // loaded grid does no render or blit work.
                    if gallery_ctl.dirty {
                        let frame = grid.render(renderer.width(), renderer.height(), queue.len());
                        if let Err(e) = renderer.show_cut(&frame, &tc) {
                            warn!("gallery render error: {e}");
                        }
                        gallery_ctl.dirty = false;
                    }
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                continue;
            }

            // Recovery: if the ring is empty (failed fetches, just opened with
            // a bad photo, etc.) try to refill before the advance path.
            if prefetched.is_empty() && !queue.is_empty() {
                self.request_extend(
                    &mut queue_io,
                    &queue_loader,
                    &queue_ids,
                    false,
                    current_queue_idx,
                    queue.len(),
                );
                Self::pump_slide_fetches(
                    &mut fetcher,
                    &queue,
                    &mut cursor,
                    &prefetched,
                    prefetch_n,
                    no_repeat_shown,
                    &shown_ids,
                );
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
                    if let Err(e) = renderer.show_cut(&black, &tc) {
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
            let mut current_slide_dur = slide_dur;
            // Thermal Throttling Guard — refresh the cached reading on its own
            // interval, then derive the interval from the cached value. Throttling
            // only ever *lengthens* the interval (never shorter than configured),
            // so a long base slide duration can't be sped up by a hot CPU.
            if last_temp_check.is_none_or(|t| t.elapsed() >= TEMP_POLL_INTERVAL) {
                cached_temp = read_cpu_temp();
                last_temp_check = Some(Instant::now());

                if let Some(temp) = cached_temp {
                    if temp >= 80.0 {
                        warn!("CPU temperature is extremely high ({:.1}°C) — throttling slide interval to cool down", temp);
                    } else if temp >= 75.0 {
                        warn!(
                            "CPU temperature is high ({:.1}°C) — throttling slide interval",
                            temp
                        );
                    } else if temp >= 70.0 {
                        warn!(
                            "CPU temperature is warm ({:.1}°C) — throttling slide interval",
                            temp
                        );
                    }
                }
            }

            if let Some(temp) = cached_temp {
                if temp >= 80.0 {
                    current_slide_dur = slide_dur.max(Duration::from_secs(60));
                } else if temp >= 75.0 {
                    current_slide_dur = slide_dur * 2;
                } else if temp >= 70.0 {
                    current_slide_dur = slide_dur + slide_dur / 2;
                }
            }

            if last_advance.elapsed() < current_slide_dur {
                self.request_extend(
                    &mut queue_io,
                    &queue_loader,
                    &queue_ids,
                    false,
                    cursor,
                    queue.len(),
                );
                Self::pump_slide_fetches(
                    &mut fetcher,
                    &queue,
                    &mut cursor,
                    &prefetched,
                    prefetch_n,
                    no_repeat_shown,
                    &shown_ids,
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // ── Display next photo ────────────────────────────────────────
            // The image is already decoded and scaled (done in the fetcher),
            // so all that's left are the cheap per-slide pixel passes and the
            // transition itself.
            if let Some((q_idx, meta, mut rgba, exif_date)) = prefetched.pop_front() {
                debug!("Showing: {}", meta.filename);
                if no_repeat_shown {
                    if shown_ids[queue[q_idx].0].insert(meta.id.clone()) {
                        shown_count += 1;
                    }
                    if shown_count >= queue.len() {
                        shown_ids.iter_mut().for_each(HashSet::clear);
                        shown_count = 0;
                        info!("All photos shown — starting a fresh no-repeat cycle");
                    }
                }
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
                // When gallery mode is on, composite onto a full-screen frame
                // before drawing the × close control so its screen-space hit
                // test matches the pixels the user sees (letterboxed photos
                // would otherwise put × on the photo while hit-testing the
                // screen corner).
                let mut frame = if gallery_mode
                    && (rgba.width() != renderer.width() || rgba.height() != renderer.height())
                {
                    let mut f = RgbaImage::from_pixel(
                        renderer.width().max(1),
                        renderer.height().max(1),
                        Rgba([0, 0, 0, 255]),
                    );
                    blit_center(&mut f, &rgba);
                    f
                } else {
                    rgba
                };
                if self.config.display.show_osd {
                    crate::osd::draw_photo_info(&mut frame, &meta, exif_date.as_deref());
                    crate::osd::draw_nav_arrows(&mut frame);
                    // Mark already-favourited photos with a ♥ in the corner.
                    if meta.is_favorite {
                        crate::osd::draw_favorite(&mut frame);
                    }
                }
                // Close (×) must remain available even when OSD info is off —
                // it is the mouse path back to the gallery grid.
                if gallery_mode {
                    crate::osd::draw_close_button(&mut frame);
                }
                // Clock is its own toggle (independent of show_osd). Formatted
                // here from the wall clock at display time, so it reflects the
                // current minute each slide.
                if self.config.display.show_clock {
                    let now = chrono::Local::now().format("%H:%M").to_string();
                    crate::osd::draw_clock(&mut frame, &now);
                }
                let transition = if open_cut_once {
                    open_cut_once = false;
                    Transition::Cut
                } else {
                    self.config.display.transition.clone()
                };
                let result = match transition {
                    Transition::Cut => renderer.show_cut(&frame, &tc),
                    Transition::Fade => {
                        renderer
                            .show_fade(current_rgba.as_ref(), &frame, trans_dur)
                            .await
                    }
                    Transition::SlideLeft => {
                        renderer
                            .show_slide_left(current_rgba.as_ref(), &frame, trans_dur)
                            .await
                    }
                    Transition::SlideRight => {
                        renderer
                            .show_slide_right(current_rgba.as_ref(), &frame, trans_dur)
                            .await
                    }
                };
                if let Err(e) = result {
                    warn!("Render error: {}", e);
                }
                current_queue_idx = q_idx;
                current_rgba = Some(frame);
                // Remember the source plugin + metadata for the favourite toggle.
                let plugin_idx = queue[q_idx].0;
                current_meta = Some((plugin_idx, meta.clone()));
                last_advance = Instant::now();
                // Reflect the newly displayed photo in the remote's status endpoint.
                if let Some(status) = &remote_status {
                    let mut s = status.lock().await;
                    s.paused = paused;
                    s.index = q_idx;
                    s.total = queue.len();
                    s.filename = meta.filename.clone();
                    s.album = meta.album.clone().unwrap_or_default();
                    s.favorite = meta.is_favorite;
                }
            }

            // Top up again straight after the advance so a zero/short slide
            // duration (which never enters the idle branch above) still keeps
            // the buffer fed.
            Self::pump_slide_fetches(
                &mut fetcher,
                &queue,
                &mut cursor,
                &prefetched,
                prefetch_n,
                no_repeat_shown,
                &shown_ids,
            );
        }
    }

    fn pump_slide_fetches(
        fetcher: &mut Fetcher,
        queue: &[(usize, PhotoMeta)],
        cursor: &mut usize,
        prefetched: &VecDeque<(usize, PhotoMeta, RgbaImage, Option<String>)>,
        prefetch_n: usize,
        no_repeat: bool,
        shown_ids: &[HashSet<String>],
    ) {
        if queue.is_empty() || prefetched.len() >= prefetch_n {
            return;
        }
        let start = *cursor;
        let mut attempts = 0usize;
        while prefetched.len() < prefetch_n
            && fetcher.has_capacity()
            && attempts < queue.len().min(MAX_PREFETCH_ATTEMPTS_PER_TICK)
        {
            let idx = *cursor;
            let (pidx, meta) = &queue[idx];
            *cursor += 1;
            if *cursor >= queue.len() {
                *cursor = 0;
            }
            attempts += 1;
            if no_repeat && shown_ids[*pidx].contains(meta.id.as_str()) {
                continue;
            }
            if prefetched.iter().any(|(i, _, _, _)| *i == idx) {
                if *cursor == start {
                    break;
                }
                continue;
            }
            let _ = fetcher.try_spawn(FetchJob::Slide {
                queue_idx: idx,
                plugin_idx: *pidx,
                meta: meta.clone(),
                priority: false,
            });
            if *cursor == start {
                break;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_fetch_result(
        &self,
        done: FetchDone,
        prefetched: &mut VecDeque<(usize, PhotoMeta, RgbaImage, Option<String>)>,
        gallery_ctl: &mut GalleryController,
        fullscreen_ctl: &mut FullscreenController,
        mode: &mut Mode,
        current_queue_idx: &mut usize,
        cursor: &mut usize,
        open_cut_once: &mut bool,
        last_advance: &mut Instant,
        paused: &mut bool,
        slide_dur: Duration,
        queue: &[(usize, PhotoMeta)],
    ) {
        match done {
            FetchDone::Slide {
                queue_idx,
                meta,
                priority,
                rgba,
                exif_date,
                ..
            } => {
                if fullscreen_ctl.awaiting_open == Some(queue_idx) {
                    prefetched.push_front((queue_idx, meta, rgba, exif_date));
                    *mode = Mode::Fullscreen;
                    *paused = false;
                    fullscreen_ctl.awaiting_open = None;
                    *current_queue_idx = queue_idx;
                    if !queue.is_empty() {
                        *cursor = (queue_idx + 1) % queue.len();
                    }
                    *open_cut_once = true;
                    *last_advance = Instant::now()
                        .checked_sub(slide_dur)
                        .unwrap_or_else(Instant::now);
                    info!("Opened slideshow at photo {}.", queue_idx + 1);
                } else if priority {
                    prefetched.push_front((queue_idx, meta, rgba, exif_date));
                } else {
                    prefetched.push_back((queue_idx, meta, rgba, exif_date));
                }
            }
            FetchDone::Thumb {
                queue_idx, rgba, ..
            } => {
                gallery_ctl.grid.insert_thumb(queue_idx, rgba);
                gallery_ctl.dirty = true;
            }
            FetchDone::Failed {
                queue_idx,
                kind: JobKind::Slide,
                reason,
                ..
            } => {
                if fullscreen_ctl.awaiting_open == Some(queue_idx) {
                    *mode = Mode::Gallery;
                    gallery_ctl.enter();
                    let name = queue
                        .get(queue_idx)
                        .map(|(_, m)| m.filename.as_str())
                        .unwrap_or("?");
                    warn!(
                        "Could not open photo {} ({name}) ({reason:?}) — staying in gallery",
                        queue_idx + 1
                    );
                    fullscreen_ctl.awaiting_open = None;
                }
            }
            FetchDone::Failed {
                queue_idx,
                kind: JobKind::Thumb,
                ..
            } => {
                gallery_ctl.grid.mark_thumb_failed(queue_idx);
                gallery_ctl.dirty = true;
            }
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

    /// Build the settings-menu rows from the live config + menu edit state.
    /// Shared by the input-interpretation pass and the render pass so both see
    /// identical rows (clicks map to exactly what is drawn).
    fn build_menu_rows(
        &self,
        paused: bool,
        menu: &Menu,
        sources: &[(String, bool)],
    ) -> Vec<crate::menu::MenuRow> {
        let targeting = self.targeting_menu_ctx();
        let connections = self.connection_menu_ctx();
        crate::menu::build_rows(&crate::menu::RowsCtx {
            display: &self.config.display,
            paused,
            sources,
            wifi: &self.config.wifi,
            connections: &connections,
            targeting: targeting.as_ref(),
            editing: menu.editing,
            buffer: &menu.buffer,
        })
    }

    /// Targeting adapters from live plugins (capability-driven, no name checks).
    fn targeting_adapters(&self) -> Vec<(String, picogallery_core::TargetingAdapter)> {
        self.plugins
            .iter()
            .map(|p| (p.name().to_string(), p.capabilities().targeting))
            .collect()
    }

    /// Targeting section for the menu when any active plugin supports it.
    fn targeting_menu_ctx(&self) -> Option<crate::menu::TargetingMenuCtx<'_>> {
        let plugin = self.plugins.iter().find(|p| {
            let caps = p.capabilities();
            self.config
                .plugins
                .iter()
                .any(|e| e.enabled && e.name == p.name())
                && caps.supports_targeting()
        })?;
        let caps = plugin.capabilities();
        Some(crate::menu::TargetingMenuCtx {
            album_label: self.config.targeting.album_label(),
            favorites_only: self.config.targeting.favorites_only,
            show_favorites: caps.targeting.supports_favorites_filter(),
        })
    }

    /// Connection menu sections from plugins that advertise connection UI.
    fn connection_menu_ctx(&self) -> Vec<crate::menu::ConnectionMenuCtx> {
        let mut out = Vec::new();
        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            let caps = plugin.capabilities();
            if !caps.has_connection_ui() {
                continue;
            }
            let Some(entry) = self.config.plugins.iter().find(|e| e.name == plugin.name()) else {
                continue;
            };
            let fields = caps
                .connection_fields
                .iter()
                .map(|f| crate::menu::ConnectionMenuField {
                    key: f.key.to_string(),
                    label: f.label.to_string(),
                    secret: f.secret,
                    value: entry.config.get_str(f.key).unwrap_or("").to_string(),
                })
                .collect();
            out.push(crate::menu::ConnectionMenuCtx {
                plugin_idx,
                header: plugin.display_name().to_uppercase(),
                fields,
                reconnect_label: caps.reconnect_label.map(str::to_string),
            });
        }
        out
    }

    /// Initial buffer when a field starts being edited. Secrets start empty so
    /// the stored value is never shown; other fields preload so the user edits
    /// rather than retypes.
    fn edit_initial(&self, field: EditField) -> String {
        match field {
            EditField::WifiSsid => self.config.wifi.ssid.clone(),
            EditField::WifiPassword => String::new(),
            EditField::Connection {
                plugin_idx,
                field_idx,
            } => {
                let Some(plugin) = self.plugins.get(plugin_idx) else {
                    return String::new();
                };
                let caps = plugin.capabilities();
                let Some(desc) = caps.connection_fields.get(field_idx) else {
                    return String::new();
                };
                if desc.secret {
                    return String::new();
                }
                self.config
                    .plugins
                    .iter()
                    .find(|e| e.name == plugin.name())
                    .and_then(|e| e.config.get_str(desc.key))
                    .unwrap_or("")
                    .to_string()
            }
        }
    }

    /// Write a committed text edit back into the live config. (Persisted to disk
    /// only when the user later picks "Save settings".)
    fn commit_edit(&mut self, field: EditField, value: String) {
        match field {
            EditField::WifiSsid => self.config.wifi.ssid = value,
            EditField::WifiPassword => self.config.wifi.password = value,
            EditField::Connection {
                plugin_idx,
                field_idx,
            } => {
                let Some(plugin) = self.plugins.get(plugin_idx) else {
                    return;
                };
                let name = plugin.name().to_string();
                let caps = plugin.capabilities();
                let Some(desc) = caps.connection_fields.get(field_idx) else {
                    return;
                };
                let key = desc.key.to_string();
                if let Some(entry) = self.config.plugins.iter_mut().find(|e| e.name == name) {
                    entry
                        .config
                        .values
                        .insert(key, serde_json::Value::String(value));
                }
            }
        }
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
            // Section headers carry Noop and are never activated; harmless if one
            // somehow reaches here.
            MenuAction::Noop => MenuOutcome::Stay,
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
                match pump_sdl_until(
                    renderer,
                    Self::build_queue_with(&self.plugins, &self.config.display, true),
                )
                .await
                {
                    Ok(Ok(q)) => MenuOutcome::NewQueue(q),
                    Ok(Err(e)) => {
                        warn!("Re-order failed: {e}");
                        MenuOutcome::Stay
                    }
                    Err(quit) => quit,
                }
            }
            MenuAction::ToggleNoRepeatShown => {
                self.config.display.no_repeat_shown = !self.config.display.no_repeat_shown;
                info!(
                    "No-repeat-shown {}",
                    if self.config.display.no_repeat_shown {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                if self.config.display.no_repeat_shown {
                    MenuOutcome::ResetShown
                } else {
                    MenuOutcome::Stay
                }
            }
            MenuAction::CycleAlbum => self.cycle_album_target(renderer).await,
            MenuAction::ToggleFavoritesFilter => self.toggle_favorites_target(renderer).await,
            MenuAction::SwitchSource(idx) => self.switch_source(idx, renderer).await,
            // BeginEdit is intercepted by the caller (it needs the loop-local
            // menu state); reaching here means nothing to do.
            MenuAction::BeginEdit(_) => MenuOutcome::Stay,
            MenuAction::ToggleWifi => {
                self.config.wifi.enabled = !self.config.wifi.enabled;
                info!(
                    "Wi-Fi config {}",
                    if self.config.wifi.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                MenuOutcome::Stay
            }
            MenuAction::ApplyWifi => {
                match pump_sdl_until(renderer, crate::wifi::apply(&self.config.wifi)).await {
                    Ok(Ok(())) => info!("Wi-Fi: applied (SSID '{}')", self.config.wifi.ssid),
                    Ok(Err(e)) => warn!("Wi-Fi apply failed: {e}"),
                    Err(quit) => return quit,
                }
                MenuOutcome::Stay
            }
            MenuAction::ReconnectPlugin(plugin_idx) => {
                // Map live plugin index → config.plugins index for switch_source.
                let Some(plugin) = self.plugins.get(plugin_idx) else {
                    return MenuOutcome::Stay;
                };
                let name = plugin.name().to_string();
                match self.config.plugins.iter().position(|p| p.name == name) {
                    Some(cfg_idx) => self.switch_source(cfg_idx, renderer).await,
                    None => {
                        warn!("Reconnect: no config entry for source '{name}'");
                        MenuOutcome::Stay
                    }
                }
            }
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

    async fn apply_trial_config(
        &mut self,
        mut trial: crate::config::Config,
        renderer: &mut Renderer,
    ) -> MenuOutcome {
        let adapters = self.targeting_adapters();
        let refs: Vec<_> = adapters.iter().map(|(n, a)| (n.as_str(), *a)).collect();
        trial.apply_targeting(&refs);

        let new_plugins = (self.factory)(&trial);
        if new_plugins.is_empty() {
            return MenuOutcome::Stay;
        }

        let mut ready =
            match pump_sdl_until(renderer, init_plugins_stop_on_error(new_plugins, &trial)).await {
                Ok(Some(ready)) => ready,
                Ok(None) => return MenuOutcome::Stay,
                Err(quit) => return quit,
            };
        for plugin in &mut ready {
            match pump_sdl_until(
                renderer,
                tokio::time::timeout(AUTH_CALL_TIMEOUT, plugin.refresh_auth()),
            )
            .await
            {
                Err(quit) => {
                    shutdown_boxed(&ready).await;
                    return quit;
                }
                Ok(Err(_)) => {
                    warn!(
                        "{} auth timed out after {:?}",
                        plugin.name(),
                        AUTH_CALL_TIMEOUT
                    );
                    shutdown_boxed(&ready).await;
                    return MenuOutcome::Stay;
                }
                Ok(Ok(Err(e))) => {
                    warn!("{} auth error: {}", plugin.name(), e);
                    shutdown_boxed(&ready).await;
                    return MenuOutcome::Stay;
                }
                Ok(Ok(Ok(()))) => {}
            }
        }
        let shared = share_plugins(ready);
        match pump_sdl_until(
            renderer,
            Self::build_queue_with(&shared, &trial.display, true),
        )
        .await
        {
            Err(quit) => {
                shutdown_shared(&shared).await;
                quit
            }
            Ok(Ok(queue)) if !queue.is_empty() => {
                let old = std::mem::replace(&mut self.plugins, shared);
                self.config = trial;
                renderer.set_display_config(self.config.display.clone());
                shutdown_shared(&old).await;
                MenuOutcome::Switched(queue)
            }
            Ok(Ok(_)) => {
                warn!("Targeting filter returned no photos");
                shutdown_shared(&shared).await;
                MenuOutcome::Stay
            }
            Ok(Err(e)) => {
                warn!("Queue rebuild failed: {e}");
                shutdown_shared(&shared).await;
                MenuOutcome::Stay
            }
        }
    }

    async fn cycle_album_target(&mut self, renderer: &mut Renderer) -> MenuOutcome {
        let Some(plugin_idx) = self.plugins.iter().position(|p| {
            p.capabilities().targeting.supports_albums()
                && self
                    .config
                    .plugins
                    .iter()
                    .any(|e| e.enabled && e.name == p.name())
        }) else {
            return MenuOutcome::Stay;
        };
        let plugin = Arc::clone(&self.plugins[plugin_idx]);
        let albums = match pump_sdl_until(renderer, plugin.list_albums()).await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                warn!("Album list failed: {e}");
                return MenuOutcome::Stay;
            }
            Err(quit) => return quit,
        };
        let mut options: Vec<Option<String>> = vec![None];
        options.extend(albums.into_iter().map(|(id, _)| Some(id)));
        let current = if self.config.targeting.album.is_empty() {
            None
        } else {
            Some(self.config.targeting.album.clone())
        };
        let pos = options
            .iter()
            .position(|o| o.as_ref() == current.as_ref())
            .unwrap_or(0);
        let next = options[(pos + 1) % options.len()].clone();
        let mut trial = self.config.clone();
        trial.targeting.album = next.unwrap_or_default();
        let out = self.apply_trial_config(trial, renderer).await;
        if matches!(out, MenuOutcome::Switched(_)) {
            info!("Album target: {}", self.config.targeting.album_label());
        }
        out
    }

    async fn toggle_favorites_target(&mut self, renderer: &mut Renderer) -> MenuOutcome {
        if !self.plugins.iter().any(|p| {
            p.capabilities().targeting.supports_favorites_filter()
                && self
                    .config
                    .plugins
                    .iter()
                    .any(|e| e.enabled && e.name == p.name())
        }) {
            return MenuOutcome::Stay;
        }
        let mut trial = self.config.clone();
        trial.targeting.favorites_only = !trial.targeting.favorites_only;
        let out = self.apply_trial_config(trial, renderer).await;
        if matches!(out, MenuOutcome::Switched(_)) {
            info!(
                "Favourites-only {}",
                if self.config.targeting.favorites_only {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
        out
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
        let mut trial = self.config.clone();
        for (i, p) in trial.plugins.iter_mut().enumerate() {
            p.enabled = i == idx;
        }
        let name = trial.plugins[idx].name.clone();

        let new_plugins = (self.factory)(&trial);
        if new_plugins.is_empty() {
            warn!("Cannot switch to '{name}': not compiled into this build");
            return MenuOutcome::Stay;
        }

        let mut ready =
            match pump_sdl_until(renderer, init_plugins_stop_on_error(new_plugins, &trial)).await {
                Ok(Some(ready)) => ready,
                Ok(None) => return MenuOutcome::Stay,
                Err(quit) => return quit,
            };
        for plugin in &mut ready {
            match pump_sdl_until(
                renderer,
                tokio::time::timeout(AUTH_CALL_TIMEOUT, plugin.authenticate()),
            )
            .await
            {
                Err(quit) => {
                    shutdown_boxed(&ready).await;
                    return quit;
                }
                Ok(Ok(Ok(AuthStatus::Authenticated))) => {}
                Ok(Ok(Ok(AuthStatus::NotAuthenticated))) => {
                    warn!("Cannot switch to '{name}': not authenticated");
                    shutdown_boxed(&ready).await;
                    return MenuOutcome::Stay;
                }
                Ok(Ok(Ok(AuthStatus::PendingUserAction { .. }))) => {
                    warn!(
                        "Cannot switch to '{name}': needs interactive setup — configure it and restart"
                    );
                    shutdown_boxed(&ready).await;
                    return MenuOutcome::Stay;
                }
                Ok(Ok(Err(e))) => {
                    warn!("Cannot switch to '{name}': auth error: {e}");
                    shutdown_boxed(&ready).await;
                    return MenuOutcome::Stay;
                }
                Ok(Err(_)) => {
                    warn!(
                        "Cannot switch to '{name}': authenticate timed out after {:?}",
                        AUTH_CALL_TIMEOUT
                    );
                    shutdown_boxed(&ready).await;
                    return MenuOutcome::Stay;
                }
            }
        }
        let shared = share_plugins(ready);
        match pump_sdl_until(
            renderer,
            Self::build_queue_with(&shared, &trial.display, true),
        )
        .await
        {
            Err(quit) => {
                shutdown_shared(&shared).await;
                quit
            }
            Ok(Ok(queue)) if !queue.is_empty() => {
                info!("Switched source to '{name}' ({} photos)", queue.len());
                let old = std::mem::replace(&mut self.plugins, shared);
                self.config = trial;
                renderer.set_display_config(self.config.display.clone());
                shutdown_shared(&old).await;
                MenuOutcome::Switched(queue)
            }
            Ok(Ok(_)) => {
                warn!("Cannot switch to '{name}': source returned no photos");
                shutdown_shared(&shared).await;
                MenuOutcome::Stay
            }
            Ok(Err(e)) => {
                warn!("Cannot switch to '{name}': listing failed: {e}");
                shutdown_shared(&shared).await;
                MenuOutcome::Stay
            }
        }
    }

    /// Persist the current in-memory config back to the config file. Writes are
    /// explicit (this menu item only) — never on every toggle — to spare the
    /// Pi's SD card. Note this rewrites the file without the template's
    /// explanatory comments: the settings survive, the prose does not.
    fn save_config(&self) -> Result<()> {
        let mut to_write = self.config.clone();
        to_write.redact_secrets_for_persistence();
        let text = toml::to_string_pretty(&to_write).context("serialising config to TOML")?;
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // The file holds Wi-Fi and PhotoPrism passwords in plain text, so it must
        // not be world-readable. Write 0600 on Unix; a plain write elsewhere.
        write_private(&self.config_path, &text)
            .with_context(|| format!("writing config {}", self.config_path.display()))?;
        Config::restrict_private_permissions(&self.config_path);
        Ok(())
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

/// Atomically write `text` to `path`, owner-read/write only (0600) on Unix so
/// the saved config can't leak the stored Wi-Fi/PhotoPrism passwords to other
/// local users. Writes a sibling temp file then renames over the target, so a
/// failure mid-write never leaves the live config truncated or empty. On
/// non-Unix targets this is a plain (non-atomic) write.
#[cfg(unix)]
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Temp file in the same dir so the rename is a same-filesystem atomic swap.
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600) // created fresh each time, so mode always applies
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    // Atomic replace: readers see either the old or new complete file, at 0600.
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
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

fn apply_order(queue: &mut Vec<(usize, PhotoMeta)>, display: &DisplayConfig, seed: u64) {
    match display.order {
        PhotoOrder::Shuffle => {
            shuffle(queue, seed);
            if display.on_this_day_boost {
                weave_on_this_day(queue);
            }
        }
        PhotoOrder::Chronological => {
            queue.sort_by_key(|(_, m)| m.taken_at);
        }
        PhotoOrder::NewestFirst => {
            queue.sort_by_key(|(_, a)| std::cmp::Reverse(a.taken_at));
        }
        PhotoOrder::DateCluster => {
            *queue = date_cluster_order(std::mem::take(queue), seed);
        }
    }
}

/// Apply display ordering to a newly appended queue segment only.
///
/// Incremental pages are sorted/shuffled within the new tail — not merged back
/// into the existing queue (a full re-sort on every extension would be costly
/// on a Pi Zero). `on_this_day_boost` is not re-run for tail segments.
fn apply_order_tail(
    queue: &mut [(usize, PhotoMeta)],
    from: usize,
    display: &DisplayConfig,
    seed: u64,
) {
    if from >= queue.len() {
        return;
    }
    match display.order {
        PhotoOrder::Shuffle => {
            shuffle(&mut queue[from..], seed.wrapping_add(from as u64));
        }
        PhotoOrder::Chronological => {
            queue[from..].sort_by_key(|(_, m)| m.taken_at);
        }
        PhotoOrder::NewestFirst => {
            queue[from..].sort_by_key(|(_, a)| std::cmp::Reverse(a.taken_at));
        }
        PhotoOrder::DateCluster => {
            queue[from..].sort_by_key(|(_, m)| m.taken_at);
        }
    }
}

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
            .or_else(|| item.1.album.clone())
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

fn read_cpu_temp() -> Option<f32> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(temp_str) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
            if let Ok(temp_val) = temp_str.trim().parse::<i32>() {
                return Some(temp_val as f32 / 1000.0);
            }
        }
    }
    None
}

fn share_plugins(plugins: Vec<BoxedPlugin>) -> Vec<SharedPlugin> {
    plugins.into_iter().map(Arc::from).collect()
}

async fn shutdown_plugin(plugin: &dyn PhotoPlugin) {
    if let Err(e) = plugin.shutdown().await {
        warn!("{} shutdown failed: {e}", plugin.name());
    }
}

async fn shutdown_shared(plugins: &[SharedPlugin]) {
    for plugin in plugins {
        shutdown_plugin(plugin.as_ref()).await;
    }
}

async fn shutdown_boxed(plugins: &[BoxedPlugin]) {
    for plugin in plugins {
        shutdown_plugin(plugin.as_ref()).await;
    }
}

/// Init plugins one-by-one. On failure, shut down only plugins that reached
/// `init` (including the failing one) and leave the never-inited tail alone.
pub(crate) async fn init_plugins_stop_on_error(
    new_plugins: Vec<BoxedPlugin>,
    trial: &Config,
) -> Option<Vec<BoxedPlugin>> {
    let mut ready = Vec::new();
    for mut plugin in new_plugins {
        let name = plugin.name().to_string();
        let pcfg = trial.plugin_config(&name).cloned().unwrap_or_default();
        match plugin.init(&pcfg).await {
            Ok(()) => ready.push(plugin),
            Err(e) => {
                warn!("{name} init error: {e}");
                shutdown_plugin(plugin.as_ref()).await;
                shutdown_boxed(&ready).await;
                return None;
            }
        }
    }
    Some(ready)
}

fn draw_sign_in_osd(
    renderer: &mut Renderer,
    display_name: &str,
    message: &str,
    remaining: Option<u64>,
) {
    let tc = renderer.texture_creator();
    let mut frame = RgbaImage::from_pixel(
        renderer.width().max(1),
        renderer.height().max(1),
        Rgba([0, 0, 0, 255]),
    );
    let wait = match remaining {
        Some(secs) => format!("Waiting… {secs}s left"),
        None => "Waiting…".to_string(),
    };
    let mut labels: Vec<String> = vec![display_name.to_string()];
    labels.extend(message.lines().map(str::to_string));
    labels.push(wait);
    let rows: Vec<crate::osd::MenuItem> = labels
        .iter()
        .map(|l| crate::osd::MenuItem {
            label: l,
            is_header: true,
        })
        .collect();
    crate::osd::draw_menu(
        &mut frame,
        "PicoGallery — sign-in required",
        &rows,
        usize::MAX,
    );
    if let Err(e) = renderer.show_cut(&frame, &tc) {
        warn!("sign-in OSD: {e}");
    }
}

fn draw_stall_osd(
    renderer: &mut Renderer,
    tc: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
) {
    let mut frame = RgbaImage::from_pixel(
        renderer.width().max(1),
        renderer.height().max(1),
        Rgba([0, 0, 0, 255]),
    );
    let labels = [
        "Prefetch stalled".to_string(),
        "Retrying photo sources…".to_string(),
    ];
    let rows: Vec<crate::osd::MenuItem> = labels
        .iter()
        .map(|l| crate::osd::MenuItem {
            label: l,
            is_header: true,
        })
        .collect();
    crate::osd::draw_menu(&mut frame, "PicoGallery", &rows, usize::MAX);
    if let Err(e) = renderer.show_cut(&frame, tc) {
        warn!("stall OSD: {e}");
    }
}

enum AuthCall {
    Authenticated,
    Skip,
    Pending {
        message: String,
        poll_interval_secs: u64,
    },
}

/// One `authenticate()` attempt with the exclusive-call timeout. Caller must
/// hold the only `Arc` strong reference (`Arc::get_mut`).
async fn auth_call_once(
    plugin: &mut dyn PhotoPlugin,
    display_name: &str,
    auth_call_timeout: Duration,
) -> AuthCall {
    match tokio::time::timeout(auth_call_timeout, plugin.authenticate()).await {
        Err(_) => {
            warn!(
                "  {display_name} authenticate timed out after {:?} — source disabled",
                auth_call_timeout
            );
            shutdown_plugin(plugin).await;
            AuthCall::Skip
        }
        Ok(Err(e)) => {
            warn!("  {display_name} authentication failed: {e:#}");
            shutdown_plugin(plugin).await;
            AuthCall::Skip
        }
        Ok(Ok(AuthStatus::Authenticated)) => {
            info!("  {display_name} authenticated.");
            AuthCall::Authenticated
        }
        Ok(Ok(AuthStatus::NotAuthenticated)) => {
            warn!("  {display_name} is not authenticated; source disabled.");
            shutdown_plugin(plugin).await;
            AuthCall::Skip
        }
        Ok(Ok(AuthStatus::PendingUserAction {
            message,
            poll_interval_secs,
        })) => AuthCall::Pending {
            message,
            poll_interval_secs,
        },
    }
}

/// Sleep until the next pending-auth poll, driving OSD via `on_pending`.
/// Returns `Ok(true)` if the user quit.
async fn wait_pending_poll(
    display_name: &str,
    message: &str,
    poll_interval_secs: u64,
    pending_start: Instant,
    pending_timeout: Duration,
    on_pending: &mut impl FnMut(&str, &str, Option<u64>) -> bool,
) -> Result<bool> {
    println!("\n=== {display_name} ===\n{message}");
    println!(
        "Checking again in {} seconds…",
        poll_interval_secs.clamp(1, 60)
    );
    let deadline = if pending_timeout.is_zero() {
        None
    } else {
        Some(pending_start + pending_timeout)
    };
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Ok(false);
    }
    let sleep_cap = Duration::from_secs(poll_interval_secs.clamp(1, 60));
    let sleep_until = Instant::now() + sleep_cap;
    loop {
        let now = Instant::now();
        let remaining = deadline.map(|d| d.saturating_duration_since(now).as_secs());
        if on_pending(display_name, message, remaining) {
            return Ok(true);
        }
        if deadline.is_some_and(|d| now >= d) || now >= sleep_until {
            return Ok(false);
        }
        let mut slice = Duration::from_millis(50);
        if let Some(d) = deadline {
            slice = slice.min(d.saturating_duration_since(Instant::now()));
        }
        slice = slice.min(sleep_until.saturating_duration_since(Instant::now()));
        if slice.is_zero() {
            return Ok(false);
        }
        tokio::time::sleep(slice).await;
    }
}

/// Authenticate a plugin set. `on_pending` is called while waiting for a
/// device-code / user action; return `true` to abort (Quit).
///
/// Two-pass: every plugin gets one `authenticate()` call first so a healthy
/// source is kept even when another plugin burns the pending-user-action
/// budget. Only then do we poll the pending set.
pub(crate) async fn authenticate_plugin_set(
    plugins: Vec<SharedPlugin>,
    auth_call_timeout: Duration,
    pending_timeout: Duration,
    mut on_pending: impl FnMut(&str, &str, Option<u64>) -> bool,
) -> Result<Vec<SharedPlugin>> {
    let mut authenticated = Vec::with_capacity(plugins.len());
    // (plugin, display_name, message, poll_interval_secs, pending_start)
    let mut pending: Vec<(SharedPlugin, String, String, u64, Instant)> = Vec::new();

    // ── Pass 1: one authenticate() per plugin ─────────────────────────────
    for mut arc in plugins {
        let display_name = arc.display_name().to_string();
        info!("Authenticating plugin: {display_name}");
        let Some(plugin) = Arc::get_mut(&mut arc) else {
            warn!("  {display_name} is already shared; cannot authenticate — source disabled");
            shutdown_plugin(arc.as_ref()).await;
            continue;
        };
        match auth_call_once(plugin, &display_name, auth_call_timeout).await {
            AuthCall::Authenticated => authenticated.push(arc),
            AuthCall::Skip => {}
            AuthCall::Pending {
                message,
                poll_interval_secs,
            } => {
                pending.push((
                    arc,
                    display_name,
                    message,
                    poll_interval_secs,
                    Instant::now(),
                ));
            }
        }
    }

    // ── Pass 2: poll only plugins that need user action ───────────────────
    for (mut arc, display_name, mut message, mut poll_interval_secs, pending_start) in pending {
        let mut kept = false;
        loop {
            let deadline = if pending_timeout.is_zero() {
                None
            } else {
                Some(pending_start + pending_timeout)
            };
            if deadline.is_some_and(|d| Instant::now() >= d) {
                warn!("  {display_name} pending user action exceeded timeout — source disabled");
                shutdown_plugin(arc.as_ref()).await;
                break;
            }
            let quit = wait_pending_poll(
                &display_name,
                &message,
                poll_interval_secs,
                pending_start,
                pending_timeout,
                &mut on_pending,
            )
            .await?;
            if quit {
                shutdown_plugin(arc.as_ref()).await;
                shutdown_shared(&authenticated).await;
                anyhow::bail!("quit during sign-in");
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                warn!("  {display_name} pending user action exceeded timeout — source disabled");
                shutdown_plugin(arc.as_ref()).await;
                break;
            }

            let Some(plugin) = Arc::get_mut(&mut arc) else {
                warn!("  {display_name} is already shared; cannot authenticate — source disabled");
                shutdown_plugin(arc.as_ref()).await;
                break;
            };
            match auth_call_once(plugin, &display_name, auth_call_timeout).await {
                AuthCall::Authenticated => {
                    kept = true;
                    break;
                }
                AuthCall::Skip => break,
                AuthCall::Pending {
                    message: next_msg,
                    poll_interval_secs: next_poll,
                } => {
                    message = next_msg;
                    poll_interval_secs = next_poll;
                }
            }
        }
        if kept {
            authenticated.push(arc);
        }
    }

    if authenticated.is_empty() {
        anyhow::bail!("No providers authenticated successfully")
    }
    Ok(authenticated)
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
    fn provider_errors_use_backoff_without_permanent_exhaustion() {
        let mut loader = QueueLoader::new(1);
        let delay = loader.record_error(0);
        assert_eq!(delay, Duration::from_secs(1));
        assert!(!loader.plugin_exhausted[0]);
        assert!(!loader.ready_to_retry(0));
        loader.record_success(0);
        assert!(loader.ready_to_retry(0));
        assert!(!loader.plugin_retryable_error[0]);
    }

    fn meta_for(id: &str) -> PhotoMeta {
        PhotoMeta {
            id: id.into(),
            filename: format!("{id}.jpg"),
            width: 0,
            height: 0,
            taken_at: None,
            download_url: None,
            album: None,
            title: None,
            location: None,
            is_favorite: false,
            extra: Default::default(),
        }
    }

    #[test]
    fn sync_counts_marks_short_page_as_exhausted() {
        // Plugin 0 returned only 10 items (< PAGE_SIZE) in the single initial
        // round — it must be treated as exhausted so navigation near the end
        // doesn't waste a round-trip re-discovering that.
        let queue: Vec<_> = (0..10)
            .map(|i| (0usize, meta_for(&format!("p{i}"))))
            .collect();
        let mut loader = QueueLoader::new(1);
        loader.sync_counts(&queue);
        assert!(loader.all_exhausted());
    }

    #[test]
    fn sync_counts_leaves_full_page_not_exhausted() {
        // A full PAGE_SIZE page might not be the last one — don't assume
        // exhaustion just because the count happens to align.
        let queue: Vec<_> = (0..PAGE_SIZE)
            .map(|i| (0usize, meta_for(&format!("p{i}"))))
            .collect();
        let mut loader = QueueLoader::new(1);
        loader.sync_counts(&queue);
        assert!(!loader.all_exhausted());
    }

    #[test]
    fn sync_counts_keeps_zero_items_retryable() {
        let mut loader = QueueLoader::new(2);
        loader.sync_counts(&[]);
        assert!(!loader.all_exhausted());
    }

    #[test]
    fn near_end_false_once_all_plugins_exhausted() {
        let mut loader = QueueLoader::new(1);
        loader.mark_fully_loaded();
        assert!(!loader.near_end(0, 5));
    }

    #[test]
    fn near_end_true_within_margin_of_queue_end() {
        let loader = QueueLoader::new(1);
        assert!(loader.near_end(9, 10));
        assert!(!loader.near_end(0, 1000));
    }
    #[test]
    fn filter_unseen_photos_drops_duplicates_keeps_new() {
        let mut seen: HashSet<(usize, String)> = HashSet::new();
        seen.insert((0, "a".into()));
        let batch = vec![
            (0usize, meta_for("a")),
            (0usize, meta_for("b")),
            (1usize, meta_for("a")), // different plugin — keep
            (0usize, meta_for("b")), // dup in same batch — drop
        ];
        let fresh = filter_unseen_photos(batch, &mut seen);
        let ids: Vec<_> = fresh.iter().map(|(pi, m)| (*pi, m.id.as_str())).collect();
        assert_eq!(ids, vec![(0, "b"), (1, "a")]);
        assert!(seen.contains(&(0, "b".into())));
    }

    #[test]
    fn test_queue_loader_mark_fully_loaded() {
        let mut loader = QueueLoader::new(3);
        assert!(!loader.all_exhausted());

        loader.mark_fully_loaded();
        assert!(loader.all_exhausted());
        assert!(loader.plugin_exhausted[0]);
        assert!(loader.plugin_exhausted[1]);
        assert!(loader.plugin_exhausted[2]);
    }

    #[test]
    fn test_read_cpu_temp() {
        // Just verify it compiles and doesn't panic on the current platform
        let _temp = read_cpu_temp();
    }

    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct AuthSpy {
        name: &'static str,
        kind: AuthKind,
        shutdowns: Arc<AtomicUsize>,
        /// Optional log of Instant at each authenticate() entry (ordering tests).
        auth_calls: Option<Arc<std::sync::Mutex<Vec<Instant>>>>,
    }

    enum AuthKind {
        Pending,
        Slow,
        Ok,
        FailInit,
    }

    impl AuthSpy {
        fn new(name: &'static str, kind: AuthKind, shutdowns: Arc<AtomicUsize>) -> Self {
            Self {
                name,
                kind,
                shutdowns,
                auth_calls: None,
            }
        }
    }

    #[async_trait]
    impl PhotoPlugin for AuthSpy {
        fn name(&self) -> &str {
            self.name
        }
        async fn init(&mut self, _config: &picogallery_core::PluginConfig) -> Result<()> {
            if matches!(self.kind, AuthKind::FailInit) {
                anyhow::bail!("init failed");
            }
            Ok(())
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus::Authenticated
        }
        async fn authenticate(&mut self) -> Result<AuthStatus> {
            if let Some(log) = &self.auth_calls {
                log.lock().unwrap().push(Instant::now());
            }
            match self.kind {
                AuthKind::Pending => Ok(AuthStatus::PendingUserAction {
                    message: "visit example".into(),
                    poll_interval_secs: 1,
                }),
                AuthKind::Slow => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    Ok(AuthStatus::Authenticated)
                }
                AuthKind::Ok | AuthKind::FailInit => Ok(AuthStatus::Authenticated),
            }
        }
        async fn list_photos(&self, _limit: usize, _offset: usize) -> Result<Vec<PhotoMeta>> {
            Ok(vec![dummy_meta()])
        }
        async fn get_photo_bytes(
            &self,
            _meta: &PhotoMeta,
            _intent: picogallery_core::FetchIntent,
        ) -> Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
        async fn shutdown(&self) -> Result<()> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn no_osd(_: &str, _: &str, _: Option<u64>) -> bool {
        false
    }

    #[tokio::test]
    async fn pending_user_action_gives_up_at_the_deadline() {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let plugins = share_plugins(vec![Box::new(AuthSpy::new(
            "pending",
            AuthKind::Pending,
            Arc::clone(&shutdowns),
        ))]);
        let run = authenticate_plugin_set(
            plugins,
            Duration::from_secs(60),
            Duration::from_secs(2),
            no_osd,
        );
        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("authenticate_plugin_set hung");
        assert!(result.is_err(), "sole pending plugin must fail auth");
        assert!(shutdowns.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn slow_authenticate_call_is_timed_out() {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let plugins = share_plugins(vec![Box::new(AuthSpy::new(
            "slow",
            AuthKind::Slow,
            Arc::clone(&shutdowns),
        ))]);
        let run = authenticate_plugin_set(
            plugins,
            Duration::from_millis(50),
            Duration::from_secs(180),
            no_osd,
        );
        let result = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("slow authenticate hung the suite");
        assert!(result.is_err());
        assert!(shutdowns.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn one_pending_plugin_does_not_block_a_healthy_one() {
        let pending_sd = Arc::new(AtomicUsize::new(0));
        let ok_sd = Arc::new(AtomicUsize::new(0));
        let plugins = share_plugins(vec![
            Box::new(AuthSpy::new(
                "pending",
                AuthKind::Pending,
                Arc::clone(&pending_sd),
            )),
            Box::new(AuthSpy::new("healthy", AuthKind::Ok, Arc::clone(&ok_sd))),
        ]);
        let run = authenticate_plugin_set(
            plugins,
            Duration::from_secs(60),
            Duration::from_secs(2),
            no_osd,
        );
        let kept = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("authenticate_plugin_set hung")
            .expect("healthy plugin should keep the set alive");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name(), "healthy");
        assert!(pending_sd.load(Ordering::SeqCst) >= 1);
        assert_eq!(ok_sd.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn healthy_plugin_authenticated_before_pending_wait() {
        // Pass 1 must finish B's authenticate before pass 2 sleeps on A.
        let pending_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let healthy_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending_sd = Arc::new(AtomicUsize::new(0));
        let ok_sd = Arc::new(AtomicUsize::new(0));
        let mut pending_spy = AuthSpy::new("pending", AuthKind::Pending, Arc::clone(&pending_sd));
        pending_spy.auth_calls = Some(Arc::clone(&pending_calls));
        let mut healthy_spy = AuthSpy::new("healthy", AuthKind::Ok, Arc::clone(&ok_sd));
        healthy_spy.auth_calls = Some(Arc::clone(&healthy_calls));
        let plugins = share_plugins(vec![Box::new(pending_spy), Box::new(healthy_spy)]);
        let _kept = authenticate_plugin_set(
            plugins,
            Duration::from_secs(60),
            Duration::from_secs(2),
            no_osd,
        )
        .await
        .expect("healthy plugin should keep the set alive");
        let p = pending_calls.lock().unwrap().clone();
        let h = healthy_calls.lock().unwrap().clone();
        assert!(!h.is_empty(), "healthy authenticate must run");
        assert!(!p.is_empty(), "pending authenticate must run");
        // B's first authenticate completes in pass 1 before A's second call
        // (which only happens after pass-2 pending sleep).
        assert!(
            h[0] < p
                .get(1)
                .copied()
                .unwrap_or(Instant::now() + Duration::from_secs(60)),
            "healthy first auth ({:?}) must precede pending's second call / end of wait",
            h[0]
        );
        // Stronger: healthy finished before any pending re-poll.
        if p.len() >= 2 {
            assert!(
                h[0] < p[1],
                "healthy first auth must complete before pending's second authenticate"
            );
        }
    }

    #[tokio::test]
    async fn trial_config_shuts_down_only_initialized_plugins() {
        let s0 = Arc::new(AtomicUsize::new(0));
        let s1 = Arc::new(AtomicUsize::new(0));
        let s2 = Arc::new(AtomicUsize::new(0));
        let plugins: Vec<BoxedPlugin> = vec![
            Box::new(AuthSpy::new("p0", AuthKind::Ok, Arc::clone(&s0))),
            Box::new(AuthSpy::new("p1", AuthKind::FailInit, Arc::clone(&s1))),
            Box::new(AuthSpy::new("p2", AuthKind::Ok, Arc::clone(&s2))),
        ];
        let result = init_plugins_stop_on_error(plugins, &Config::default()).await;
        assert!(result.is_none());
        assert_eq!(s0.load(Ordering::SeqCst), 1);
        assert_eq!(s1.load(Ordering::SeqCst), 1);
        assert_eq!(s2.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn slideshow_new_starts_with_unwritable_cache_dir() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!(
            "picogallery-ss-unwritable-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let as_file = tmp.join("not-a-dir");
        std::fs::write(&as_file, b"x").unwrap();
        let mut config = Config::default();
        config.cache.dir = Some(as_file.join("cache"));
        let slideshow = Slideshow::new(
            config,
            Vec::new(),
            tmp.join("config.toml"),
            Box::new(|_: &Config| Vec::new()),
            DisplayEnv::default(),
        )
        .await
        .expect("unwritable cache must not fail startup");
        assert!(!slideshow.cache.is_enabled());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    struct MockFetchPlugin {
        name: String,
        calls: Arc<AtomicUsize>,
        pages: Arc<tokio::sync::Mutex<Vec<Result<Vec<PhotoMeta>>>>>,
    }

    #[async_trait]
    impl PhotoPlugin for MockFetchPlugin {
        fn name(&self) -> &str {
            &self.name
        }
        async fn init(&mut self, _config: &picogallery_core::PluginConfig) -> Result<()> {
            Ok(())
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus::Authenticated
        }
        async fn authenticate(&mut self) -> Result<AuthStatus> {
            Ok(AuthStatus::Authenticated)
        }
        async fn list_photos(&self, _limit: usize, _offset: usize) -> Result<Vec<PhotoMeta>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut pages = self.pages.lock().await;
            if pages.is_empty() {
                Ok(vec![])
            } else {
                pages.remove(0)
            }
        }
        async fn get_photo_bytes(
            &self,
            _meta: &PhotoMeta,
            _intent: picogallery_core::FetchIntent,
        ) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn shutdown(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn fetch_queue_round_skips_exhausted_plugins() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pages = Arc::new(tokio::sync::Mutex::new(vec![Ok(vec![dummy_meta()])]));
        let p = MockFetchPlugin {
            name: "test".into(),
            calls: Arc::clone(&calls),
            pages,
        };
        let mut loader = QueueLoader::new(1);
        loader.plugin_exhausted[0] = true;
        let plugins: Vec<SharedPlugin> = vec![Arc::new(p)];
        let batch = Slideshow::fetch_queue_round(&plugins, &mut loader).await;
        assert!(batch.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fetch_queue_round_marks_exhausted_on_empty_page() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pages = Arc::new(tokio::sync::Mutex::new(vec![Ok(vec![])]));
        let p = MockFetchPlugin {
            name: "test".into(),
            calls: Arc::clone(&calls),
            pages,
        };
        let mut loader = QueueLoader::new(1);
        let plugins: Vec<SharedPlugin> = vec![Arc::new(p)];
        let batch = Slideshow::fetch_queue_round(&plugins, &mut loader).await;
        assert!(batch.is_empty());
        assert!(loader.plugin_exhausted[0]);
    }

    #[tokio::test]
    async fn fetch_queue_round_backs_off_on_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pages = Arc::new(tokio::sync::Mutex::new(vec![Err(anyhow::anyhow!(
            "network error"
        ))]));
        let p = MockFetchPlugin {
            name: "test".into(),
            calls: Arc::clone(&calls),
            pages,
        };
        let mut loader = QueueLoader::new(1);
        let plugins: Vec<SharedPlugin> = vec![Arc::new(p)];

        // First call errors out
        let batch = Slideshow::fetch_queue_round(&plugins, &mut loader).await;
        assert!(batch.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!loader.plugin_exhausted[0]);
        assert!(loader.plugin_retryable_error[0]);
        assert!(!loader.ready_to_retry(0));

        // Immediate second call should be skipped due to backoff
        let batch2 = Slideshow::fetch_queue_round(&plugins, &mut loader).await;
        assert!(batch2.is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Plugin should not be called while in backoff"
        );
    }

    #[tokio::test]
    async fn fetch_queue_round_fetches_until_max_photos() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut p_pages = vec![];
        // 2000 photos per plugin is the max. PAGE_SIZE is 50.
        // We'll simulate fetching 2000 items.
        for _ in 0..41 {
            p_pages.push(Ok(vec![dummy_meta(); 50]));
        }
        let pages = Arc::new(tokio::sync::Mutex::new(p_pages));
        let p = MockFetchPlugin {
            name: "test".into(),
            calls: Arc::clone(&calls),
            pages,
        };
        let mut loader = QueueLoader::new(1);
        let plugins: Vec<SharedPlugin> = vec![Arc::new(p)];

        for _ in 0..40 {
            let batch = Slideshow::fetch_queue_round(&plugins, &mut loader).await;
            assert_eq!(batch.len(), 50);
        }

        assert!(
            loader.plugin_exhausted[0],
            "Should be marked exhausted at 2000"
        );
    }
    #[tokio::test]
    async fn try_spawn_extend_merges_into_queue() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pages = Arc::new(tokio::sync::Mutex::new(vec![Ok(vec![dummy_meta()])]));
        let p = MockFetchPlugin {
            name: "test".into(),
            calls: Arc::clone(&calls),
            pages,
        };

        let config = Config::default();
        let cache_dir = std::env::temp_dir().join("picogallery-cache-test");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache = crate::cache::CacheHandle::open_or_degrade(&cache_dir, 100).await;

        let slideshow = Slideshow {
            config,
            plugins: vec![Arc::new(p)],
            cache,
            config_path: "dummy.toml".into(),
            factory: Box::new(|_| vec![]),
            display_env: DisplayEnv::default(),
        };

        let mut queue = Vec::new();
        let mut queue_ids = HashSet::new();
        let mut loader = QueueLoader::new(1);
        let mut queue_io = QueueIo::new();

        assert!(queue_io.try_spawn_extend(
            slideshow.plugins.clone(),
            loader.clone(),
            queue_ids.clone(),
            EXTEND_MAX_ROUNDS,
            PAGE_SIZE,
            MAX_PHOTOS_PER_PLUGIN,
        ));

        let mut done = None;
        for _ in 0..64 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            if let Some(r) = queue_io.drain_extend() {
                done = Some(r);
                break;
            }
        }
        let done = done.expect("extend should complete");
        let extended = slideshow
            .apply_extend_result(done, &mut queue, &mut queue_ids, &mut loader, &None)
            .await;
        assert!(extended, "Should successfully extend the queue");
        assert_eq!(queue.len(), 1);

        // Exhausted loader: force request must not spawn.
        assert!(loader.all_exhausted());
        slideshow.request_extend(&mut queue_io, &loader, &queue_ids, true, 0, queue.len());
        assert!(!queue_io.extend_in_flight());

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn try_spawn_extend_skips_when_not_near_end() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pages = Arc::new(tokio::sync::Mutex::new(vec![Ok(vec![dummy_meta()])]));
        let p = MockFetchPlugin {
            name: "test".into(),
            calls: Arc::clone(&calls),
            pages,
        };

        let config = Config::default();
        let cache_dir = std::env::temp_dir().join("picogallery-cache-test-not-near-end");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache = crate::cache::CacheHandle::open_or_degrade(&cache_dir, 100).await;

        let slideshow = Slideshow {
            config,
            plugins: vec![Arc::new(p)],
            cache,
            config_path: "dummy.toml".into(),
            factory: Box::new(|_| vec![]),
            display_env: DisplayEnv::default(),
        };

        let queue = vec![(0, dummy_meta()); 100];
        let queue_ids = HashSet::new();
        let loader = QueueLoader::new(1);
        let mut queue_io = QueueIo::new();

        slideshow.request_extend(&mut queue_io, &loader, &queue_ids, false, 0, queue.len());
        assert!(
            !queue_io.extend_in_flight(),
            "Should not spawn extend when not near end"
        );
        assert_eq!(queue.len(), 100);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
