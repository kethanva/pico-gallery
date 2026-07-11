/// Slideshow engine.
///
/// Runs on the Tokio runtime. A background task pre-fetches the next N images
/// while the current one is on screen, so transitions are instant on slow Pi
/// Zero I/O.  All plugin calls are async and non-blocking.
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use log::{debug, info, warn};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::cache::ImageCache;
use crate::compose::blit_center;
use crate::config::{Config, DisplayConfig, PhotoOrder, Transition};
use crate::fullscreen_controller::FullscreenController;
use crate::gallery_controller::GalleryController;
use crate::menu::{EditField, Menu, MenuAction};
use crate::mode::Mode;
use crate::plugin::{AuthStatus, BoxedPlugin, PhotoMeta};
use crate::remote::SharedStatus;
use crate::renderer::{Renderer, SlideshowCmd};
use tokio::sync::mpsc::Receiver;

const PAGE_SIZE: usize = 50; // photos fetched per API page
/// Hard cap per plugin when paging the remote library into the play queue.
const MAX_PHOTOS_PER_PLUGIN: usize = 2000;
/// Fetch the next API page when navigation is within this many items of the end.
const LOAD_AHEAD_MARGIN: usize = 30;
/// Thumbnails decoded per gallery tick — balances Pi Zero CPU with fast fill.
const GALLERY_THUMBS_PER_TICK: usize = 4;

/// Tracks how many photos have been pulled from each plugin so far.
struct QueueLoader {
    plugin_offsets: Vec<usize>,
    plugin_exhausted: Vec<bool>,
    shuffle_seed: u64,
}

impl QueueLoader {
    fn new(plugin_count: usize) -> Self {
        Self {
            plugin_offsets: vec![0; plugin_count],
            plugin_exhausted: vec![false; plugin_count],
            shuffle_seed: shuffle_seed_now(),
        }
    }

    fn all_exhausted(&self) -> bool {
        self.plugin_exhausted.iter().all(|&e| e)
    }

    fn mark_fully_loaded(&mut self) {
        self.plugin_exhausted.fill(true);
    }

    /// Recompute per-plugin item counts from an externally-built `queue` (e.g.
    /// the single-round initial load). A count of zero, or one that isn't an
    /// exact multiple of `PAGE_SIZE`, means the last page fetched for that
    /// plugin was short — i.e. the plugin already signalled exhaustion during
    /// that fetch — so mark it exhausted here too rather than re-discovering
    /// that with a wasted round-trip the first time navigation nears the end.
    fn sync_counts(&mut self, queue: &[(usize, PhotoMeta)]) {
        self.plugin_offsets.fill(0);
        for (pi, _) in queue {
            if *pi < self.plugin_offsets.len() {
                self.plugin_offsets[*pi] += 1;
            }
        }
        for (i, off) in self.plugin_offsets.iter().enumerate() {
            if *off >= MAX_PHOTOS_PER_PLUGIN || *off == 0 || *off % PAGE_SIZE != 0 {
                self.plugin_exhausted[i] = true;
            }
        }
    }

    fn near_end(&self, trigger_idx: usize, queue_len: usize) -> bool {
        !self.all_exhausted()
            && queue_len > 0
            && trigger_idx + LOAD_AHEAD_MARGIN >= queue_len.saturating_sub(1)
    }
}

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
    /// Clear the no-repeat-shown memory without rebuilding the queue.
    ResetShown,
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

        // 2. Build the initial play queue (first API page per plugin — more load
        // on demand as the user pages through the gallery or slideshow).
        let queue = self.build_queue().await?;
        if queue.is_empty() {
            anyhow::bail!(
                "No photos found across all plugins. Check your config and photo source \
                 (PhotoPrism URL/credentials, or add images to the directory plugin path). \
                 Run: journalctl -u picogallery -n 50"
            );
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
        Self::build_queue_with(&self.plugins, &self.config.display, false).await
    }

    /// Build the play queue from an explicit plugin set and display config.
    /// `full = false` loads one API page per plugin (fast gallery startup);
    /// `full = true` pages until each plugin is exhausted (menu re-order / switch).
    async fn build_queue_with(
        plugins: &[BoxedPlugin],
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
        plugins: &[BoxedPlugin],
        loader: &mut QueueLoader,
    ) -> Vec<(usize, PhotoMeta)> {
        let mut batch = Vec::new();
        for (plugin_idx, plugin) in plugins.iter().enumerate() {
            if loader.plugin_exhausted[plugin_idx] {
                continue;
            }
            let offset = loader.plugin_offsets[plugin_idx];
            if offset >= MAX_PHOTOS_PER_PLUGIN {
                loader.plugin_exhausted[plugin_idx] = true;
                continue;
            }
            match plugin.list_photos(PAGE_SIZE, offset).await {
                Ok(page) if page.is_empty() => {
                    loader.plugin_exhausted[plugin_idx] = true;
                }
                Ok(page) => {
                    info!(
                        "  {} loaded {} photos (offset {})",
                        plugin.name(),
                        page.len(),
                        offset
                    );
                    let n = page.len();
                    loader.plugin_offsets[plugin_idx] += n;
                    batch.extend(page.into_iter().map(|m| (plugin_idx, m)));
                    if n < PAGE_SIZE || loader.plugin_offsets[plugin_idx] >= MAX_PHOTOS_PER_PLUGIN {
                        loader.plugin_exhausted[plugin_idx] = true;
                    }
                }
                Err(e) => {
                    warn!("  {} list_photos error: {}", plugin.name(), e);
                    loader.plugin_exhausted[plugin_idx] = true;
                }
            }
        }
        batch
    }

    /// Append the next API page when the user is near the end of the queue.
    async fn try_extend_queue(
        &self,
        queue: &mut Vec<(usize, PhotoMeta)>,
        loader: &mut QueueLoader,
        trigger_idx: usize,
        remote_status: &Option<SharedStatus>,
    ) -> bool {
        if !loader.near_end(trigger_idx, queue.len()) {
            return false;
        }
        self.extend_queue_once(queue, loader, remote_status).await
    }

    /// Always try to append the next API page (e.g. user hit the last row/photo).
    async fn try_extend_queue_force(
        &self,
        queue: &mut Vec<(usize, PhotoMeta)>,
        loader: &mut QueueLoader,
        remote_status: &Option<SharedStatus>,
    ) -> bool {
        if loader.all_exhausted() {
            return false;
        }
        self.extend_queue_once(queue, loader, remote_status).await
    }

    async fn extend_queue_once(
        &self,
        queue: &mut Vec<(usize, PhotoMeta)>,
        loader: &mut QueueLoader,
        remote_status: &Option<SharedStatus>,
    ) -> bool {
        let before = queue.len();
        let batch = Self::fetch_queue_round(&self.plugins, loader).await;
        if batch.is_empty() {
            return false;
        }
        let from = queue.len();
        queue.extend(batch);
        apply_order_tail(queue, from, &self.config.display, loader.shuffle_seed);
        info!(
            "Loaded {} more photos ({} total)",
            queue.len() - before,
            queue.len()
        );
        if let Some(status) = remote_status {
            status.lock().unwrap_or_else(|e| e.into_inner()).total = queue.len();
        }
        true
    }

    // ── Display loop ──────────────────────────────────────────────────────

    async fn display_loop(
        &mut self,
        renderer: &mut Renderer,
        mut queue: Vec<(usize, PhotoMeta)>,
        mut remote_rx: Option<Receiver<SlideshowCmd>>,
        remote_status: Option<SharedStatus>,
    ) -> Result<()> {
        let mut queue_loader = QueueLoader::new(self.plugins.len());
        queue_loader.sync_counts(&queue);
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
        let mut shown_ids: HashSet<String> = HashSet::new();
        // First frame after opening from the grid uses Cut (no fade from grid).
        let mut open_cut_once = false;

        // Pre-warm the prefetch ring (fetch + decode the first N photos).
        let mut cursor = 0usize;
        if mode.is_fullscreen() {
            for _ in 0..prefetch_n {
                self.prefetch_one(
                    &queue,
                    &mut cursor,
                    &mut prefetched,
                    prefetch_n,
                    renderer,
                    no_repeat_shown,
                    &shown_ids,
                )
                .await;
            }
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
                            if mode.is_gallery() {
                                // Grid repaints on the next tick to cover the menu.
                                gallery_ctl.dirty = true;
                            } else if let Some(img) = &current_rgba {
                                // Repaint the photo underneath so the menu vanishes.
                                let _ = renderer.show_cut(img);
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
                        if !menu.open && mode.is_fullscreen() {
                            paused = !paused;
                            info!("Slideshow {}.", if paused { "paused" } else { "resumed" });
                            last_advance = Instant::now();
                            if let Some(status) = &remote_status {
                                status.lock().unwrap_or_else(|e| e.into_inner()).paused = paused;
                            }
                        }
                    }
                    SlideshowCmd::Next => {
                        if !menu.open && mode.is_fullscreen() {
                            if current_queue_idx + 1 >= queue.len() {
                                self.try_extend_queue_force(
                                    &mut queue,
                                    &mut queue_loader,
                                    &remote_status,
                                )
                                .await;
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
                            cursor = current_queue_idx;
                            last_advance = Instant::now()
                                .checked_sub(slide_dur)
                                .unwrap_or_else(Instant::now);
                        }
                    }
                    SlideshowCmd::ToggleFavorite => {
                        if !menu.open && mode.is_fullscreen() {
                            self.toggle_favorite(&mut current_meta, &remote_status)
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
                    SlideshowCmd::GalleryClick { x, y } => {
                        if gallery_mode && mode.is_gallery() {
                            let grid = &mut gallery_ctl.grid;
                            if let Some(idx) = grid.index_at(x, y, queue.len()) {
                                grid.set_selected(idx, queue.len());
                                fullscreen_ctl.pending_open = Some(idx);
                            }
                        }
                    }
                    SlideshowCmd::GalleryPage(dir) => {
                        if mode.is_gallery() && gallery_mode {
                            let grid = &mut gallery_ctl.grid;
                            let mut scrolled =
                                grid.scroll_page(dir, queue.len(), renderer.height());
                            if !scrolled
                                && dir > 0
                                && grid.at_scroll_bottom(queue.len(), renderer.height())
                                && self
                                    .try_extend_queue_force(
                                        &mut queue,
                                        &mut queue_loader,
                                        &remote_status,
                                    )
                                    .await
                            {
                                scrolled =
                                    grid.scroll_page(dir, queue.len(), renderer.height());
                            }
                            if scrolled {
                                gallery_ctl.dirty = true;
                            }
                        }
                    }
                    SlideshowCmd::GalleryMoveSelection { dx, dy } => {
                        if mode.is_gallery() && gallery_mode {
                            let grid = &mut gallery_ctl.grid;
                            let blocked = grid.move_selection(dx, dy, queue.len());
                            if blocked
                                && self
                                    .try_extend_queue_force(
                                        &mut queue,
                                        &mut queue_loader,
                                        &remote_status,
                                    )
                                    .await
                            {
                                let _ = grid.move_selection(dx, dy, queue.len());
                            }
                            grid.ensure_selected_visible(renderer.height(), queue.len());
                            gallery_ctl.dirty = true;
                        }
                    }
                    SlideshowCmd::GalleryScroll(delta) => {
                        if mode.is_gallery() && gallery_mode {
                            let grid = &mut gallery_ctl.grid;
                            let before = grid.scroll_y;
                            grid.scroll_by(delta, queue.len(), renderer.height());
                            if grid.scroll_y != before {
                                gallery_ctl.dirty = true;
                            }
                        }
                    }
                    SlideshowCmd::GalleryOpenSelected => {
                        if gallery_mode && mode.is_gallery() && !queue.is_empty() {
                            let idx = gallery_ctl.grid.selected.min(queue.len() - 1);
                            fullscreen_ctl.pending_open = Some(idx);
                        }
                    }
                    SlideshowCmd::GalleryOpenVisible => {
                        if gallery_mode && mode.is_gallery() && !queue.is_empty() {
                            let idx = gallery_ctl.grid.selected.min(queue.len() - 1);
                            fullscreen_ctl.pending_open = Some(idx);
                        }
                    }
                }
            }

            // Open the selected photo: load THAT index into the prefetch ring
            // first so a decode failure on it cannot silently show a neighbour.
            if let Some(idx) = fullscreen_ctl.pending_open.take() {
                if idx < queue.len() {
                    prefetched.clear();
                    current_rgba = None;
                    self.load_photo_into_prefetch(&queue, idx, &mut prefetched, renderer)
                        .await;
                    if prefetched.is_empty() {
                        // Keep the user on the grid rather than advancing to a
                        // different photo they did not click.
                        mode = Mode::Gallery;
                        gallery_ctl.enter();
                        gallery_ctl.mark_dirty();
                        gallery_ctl.dirty = true;
                        warn!(
                            "Could not open photo {} ({}) — staying in gallery",
                            idx + 1,
                            queue[idx].1.filename
                        );
                    } else {
                        mode = Mode::Fullscreen;
                        paused = false;
                        current_queue_idx = idx;
                        cursor = (idx + 1) % queue.len();
                        open_cut_once = true;
                        // Top up the rest of the ring from the following photos.
                        for _ in 0..prefetch_n.saturating_sub(1) {
                            self.prefetch_one(
                                &queue,
                                &mut cursor,
                                &mut prefetched,
                                prefetch_n,
                                renderer,
                                no_repeat_shown,
                                &shown_ids,
                            )
                            .await;
                        }
                        last_advance = Instant::now()
                            .checked_sub(slide_dur)
                            .unwrap_or_else(Instant::now);
                        info!("Opened slideshow at photo {}.", idx + 1);
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
                            self.cache.lock().await.flush().await;
                            return Ok(());
                        }
                        MenuOutcome::ReloadFrames => {
                            prefetched.clear();
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
                            shown_ids.clear();
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
                            shown_ids.clear();
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
                            shown_ids.clear();
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
                    let mut frame = RgbaImage::from_pixel(
                        renderer.width().max(1),
                        renderer.height().max(1),
                        Rgba([0, 0, 0, 255]),
                    );
                    if let Some(img) = &current_rgba {
                        blit_center(&mut frame, img);
                    }
                    crate::osd::draw_menu(&mut frame, MENU_TITLE, &items, menu.selected);
                    if let Err(e) = renderer.show_cut(&frame) {
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
                    if self
                        .try_extend_queue(&mut queue, &mut queue_loader, sel, &remote_status)
                        .await
                    {
                        gallery_ctl.dirty = true;
                    }
                    // Always load the selected thumb first so it is visible.
                    if !queue.is_empty() && grid.thumb(sel).is_none() {
                        let (pidx, meta) = &queue[sel];
                        let thumb_px = grid.cell;
                        if let Some(bytes) = self
                            .fetch_photo_thumb(*pidx, meta, thumb_px, renderer)
                            .await
                        {
                            let processor = renderer.image_processor();
                            let decoded = tokio::task::spawn_blocking(move || {
                                processor.decode_thumbnail(&bytes, thumb_px)
                            })
                            .await;
                            if let Ok(Ok(img)) = decoded {
                                grid.insert_thumb(sel, img);
                                gallery_ctl.dirty = true;
                            }
                        }
                    }
                    // Load missing thumbs for visible cells (batched per tick).
                    let visible = grid.visible_indices(renderer.height(), queue.len());
                    if !visible.is_empty() {
                        let start = gallery_thumb_cursor % visible.len();
                        let mut loaded = 0usize;
                        for offset in 0..visible.len() {
                            if loaded >= GALLERY_THUMBS_PER_TICK {
                                break;
                            }
                            let pick = visible[(start + offset) % visible.len()];
                            if grid.thumb(pick).is_some() {
                                continue;
                            }
                            gallery_thumb_cursor = (start + offset + 1) % visible.len();
                            let (pidx, meta) = &queue[pick];
                            let thumb_px = grid.cell;
                            if let Some(bytes) = self
                                .fetch_photo_thumb(*pidx, meta, thumb_px, renderer)
                                .await
                            {
                                let processor = renderer.image_processor();
                                let decoded = tokio::task::spawn_blocking(move || {
                                    processor.decode_thumbnail(&bytes, thumb_px)
                                })
                                .await;
                                if let Ok(Ok(img)) = decoded {
                                    grid.insert_thumb(pick, img);
                                    gallery_ctl.dirty = true;
                                    loaded += 1;
                                }
                            }
                        }
                    }
                    // Only repaint when something changed — an idle, fully
                    // loaded grid does no render or blit work.
                    if gallery_ctl.dirty {
                        let frame = grid.render(renderer.width(), renderer.height(), queue.len());
                        if let Err(e) = renderer.show_cut(&frame) {
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
                if queue_loader.near_end(current_queue_idx, queue.len()) {
                    self.try_extend_queue(
                        &mut queue,
                        &mut queue_loader,
                        current_queue_idx,
                        &remote_status,
                    )
                    .await;
                }
                for _ in 0..prefetch_n {
                    self.prefetch_one(
                        &queue,
                        &mut cursor,
                        &mut prefetched,
                        prefetch_n,
                        renderer,
                        no_repeat_shown,
                        &shown_ids,
                    )
                    .await;
                    if !prefetched.is_empty() {
                        break;
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
                if queue_loader.near_end(cursor, queue.len()) {
                    self.try_extend_queue(&mut queue, &mut queue_loader, cursor, &remote_status)
                        .await;
                }
                self.prefetch_one(
                    &queue,
                    &mut cursor,
                    &mut prefetched,
                    prefetch_n,
                    renderer,
                    no_repeat_shown,
                    &shown_ids,
                )
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
                if no_repeat_shown {
                    shown_ids.insert(photo_shown_key(queue[q_idx].0, &meta));
                    if queue
                        .iter()
                        .all(|(pi, m)| shown_ids.contains(&photo_shown_key(*pi, m)))
                    {
                        shown_ids.clear();
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
                    Transition::Cut => renderer.show_cut(&frame),
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
                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
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
            self.prefetch_one(
                &queue,
                &mut cursor,
                &mut prefetched,
                prefetch_n,
                renderer,
                no_repeat_shown,
                &shown_ids,
            )
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
    /// Fetch + decode a specific queue index into the front of the prefetch
    /// ring. Used when opening a photo from the gallery so the clicked photo
    /// is what appears — not a neighbour that happened to decode first.
    async fn load_photo_into_prefetch(
        &self,
        queue: &[(usize, PhotoMeta)],
        idx: usize,
        prefetched: &mut VecDeque<(usize, PhotoMeta, RgbaImage, Option<String>)>,
        renderer: &Renderer,
    ) {
        if idx >= queue.len() {
            return;
        }
        let (pidx, meta) = &queue[idx];
        let Some(bytes) = self.fetch_photo(*pidx, meta, renderer).await else {
            return;
        };
        // Decode off the async runtime — a full-res decode is CPU-heavy and
        // would otherwise stall events, the HTTP remote, and gallery loading
        // on the single-threaded executor.
        let processor = renderer.image_processor();
        match tokio::task::spawn_blocking(move || processor.decode_and_scale(&bytes)).await {
            Ok(Ok((rgba, exif_date))) => {
                prefetched.push_front((idx, meta.clone(), rgba, exif_date));
            }
            Ok(Err(e)) => warn!("Decode error ({}): {}", meta.filename, e),
            Err(e) => warn!("Decode task failed ({}): {}", meta.filename, e),
        }
    }

    #[allow(clippy::too_many_arguments)] // prefetch-ring state fan-out; a struct would only add ceremony
    async fn prefetch_one(
        &self,
        queue: &[(usize, PhotoMeta)],
        cursor: &mut usize,
        prefetched: &mut VecDeque<(usize, PhotoMeta, RgbaImage, Option<String>)>,
        prefetch_n: usize,
        renderer: &Renderer,
        no_repeat: bool,
        shown_ids: &HashSet<String>,
    ) {
        if prefetched.len() >= prefetch_n || queue.is_empty() {
            return;
        }
        let start = *cursor;
        let mut attempts = 0usize;
        while prefetched.len() < prefetch_n && attempts < queue.len() {
            let idx = *cursor;
            let (pidx, meta) = &queue[idx];
            *cursor += 1;
            if *cursor >= queue.len() {
                *cursor = 0;
            }
            attempts += 1;

            if no_repeat && shown_ids.contains(&photo_shown_key(*pidx, meta)) {
                continue;
            }

            let Some(bytes) = self.fetch_photo(*pidx, meta, renderer).await else {
                if *cursor == start {
                    break;
                }
                continue;
            };
            // Decode on a blocking thread (see load_photo_into_prefetch).
            let processor = renderer.image_processor();
            match tokio::task::spawn_blocking(move || processor.decode_and_scale(&bytes)).await {
                Ok(Ok((rgba, exif_date))) => {
                    prefetched.push_back((idx, meta.clone(), rgba, exif_date));
                    return;
                }
                Ok(Err(e)) => warn!("Decode error ({}): {}", meta.filename, e),
                Err(e) => warn!("Decode task failed ({}): {}", meta.filename, e),
            }
            if *cursor == start {
                break;
            }
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
        // Skip sources that don't advertise a per-photo favourite toggle —
        // calling set_favorite would just hit the trait's "unsupported" default
        // and log a misleading failure warning for e.g. directory / WebDAV.
        if !self.plugins[*plugin_idx].capabilities().favorite_toggle {
            debug!(
                "Favourite toggle ignored — source '{}' has no favourite support",
                self.plugins[*plugin_idx].name()
            );
            return;
        }
        let currently = meta.is_favorite;
        let target = !currently;

        match self.plugins[*plugin_idx].set_favorite(&*meta, target).await {
            Ok(()) => {
                meta.is_favorite = target;
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

    fn apply_targeting_now(&mut self) {
        let adapters = self.targeting_adapters();
        let refs: Vec<_> = adapters.iter().map(|(n, a)| (n.as_str(), *a)).collect();
        self.config.apply_targeting(&refs);
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
                match Self::build_queue_with(&self.plugins, &self.config.display, true).await {
                    Ok(q) => MenuOutcome::NewQueue(q),
                    Err(e) => {
                        warn!("Re-order failed: {e}");
                        MenuOutcome::Stay
                    }
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
            MenuAction::CycleAlbum => self.cycle_album_target().await,
            MenuAction::ToggleFavoritesFilter => self.toggle_favorites_target().await,
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
                match crate::wifi::apply(&self.config.wifi).await {
                    Ok(()) => info!("Wi-Fi: applied (SSID '{}')", self.config.wifi.ssid),
                    Err(e) => warn!("Wi-Fi apply failed: {e}"),
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

    async fn reload_plugins_after_targeting(&mut self) -> Result<()> {
        for plugin in &mut self.plugins {
            let pcfg = self
                .config
                .plugin_config(plugin.name())
                .cloned()
                .unwrap_or_default();
            plugin.init(&pcfg).await?;
            plugin.refresh_auth().await?;
        }
        Ok(())
    }

    async fn cycle_album_target(&mut self) -> MenuOutcome {
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
        let albums = match self.plugins[plugin_idx].list_albums().await {
            Ok(a) => a,
            Err(e) => {
                warn!("Album list failed: {e}");
                return MenuOutcome::Stay;
            }
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
        self.config.targeting.album = next.unwrap_or_default();
        self.apply_targeting_now();
        if let Err(e) = self.reload_plugins_after_targeting().await {
            warn!("Reload after album change failed: {e}");
            return MenuOutcome::Stay;
        }
        match Self::build_queue_with(&self.plugins, &self.config.display, true).await {
            Ok(q) if !q.is_empty() => {
                info!("Album target: {}", self.config.targeting.album_label());
                MenuOutcome::NewQueue(q)
            }
            Ok(_) => {
                warn!("Album filter returned no photos");
                MenuOutcome::Stay
            }
            Err(e) => {
                warn!("Queue rebuild failed: {e}");
                MenuOutcome::Stay
            }
        }
    }

    async fn toggle_favorites_target(&mut self) -> MenuOutcome {
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
        self.config.targeting.favorites_only = !self.config.targeting.favorites_only;
        self.apply_targeting_now();
        if let Err(e) = self.reload_plugins_after_targeting().await {
            warn!("Reload after favourites toggle failed: {e}");
            return MenuOutcome::Stay;
        }
        match Self::build_queue_with(&self.plugins, &self.config.display, true).await {
            Ok(q) if !q.is_empty() => {
                info!(
                    "Favourites-only {}",
                    if self.config.targeting.favorites_only {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                MenuOutcome::NewQueue(q)
            }
            Ok(_) => {
                warn!("Favourites filter returned no photos");
                MenuOutcome::Stay
            }
            Err(e) => {
                warn!("Queue rebuild failed: {e}");
                MenuOutcome::Stay
            }
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

        match Self::build_queue_with(&new_plugins, &trial.display, true).await {
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
        // The file holds Wi-Fi and PhotoPrism passwords in plain text, so it must
        // not be world-readable. Write 0600 on Unix; a plain write elsewhere.
        write_private(&self.config_path, &text)
            .with_context(|| format!("writing config {}", self.config_path.display()))?;
        Config::restrict_private_permissions(&self.config_path);
        Ok(())
    }

    // ── Fetching ──────────────────────────────────────────────────────────

    async fn fetch_photo_thumb(
        &self,
        plugin_idx: usize,
        meta: &PhotoMeta,
        thumb_px: u32,
        _renderer: &Renderer,
    ) -> Option<Vec<u8>> {
        let plugin = &self.plugins[plugin_idx];
        let cache_key = format!("{}/thumb/{}", plugin.name(), meta.id);

        if let Some(bytes) = self.cache.lock().await.get(&cache_key).await {
            return Some(bytes);
        }

        let fetch = plugin.get_photo_bytes(meta, thumb_px, thumb_px);
        match tokio::time::timeout(Duration::from_secs(30), fetch).await {
            Ok(Ok(bytes)) => {
                let _ = self.cache.lock().await.put(&cache_key, &bytes).await;
                Some(bytes)
            }
            Ok(Err(e)) => {
                warn!("fetch_photo_thumb {} error: {}", meta.filename, e);
                None
            }
            Err(_) => {
                warn!("fetch_photo_thumb {} timed out after 30 s", meta.filename);
                None
            }
        }
    }

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

fn photo_shown_key(plugin_idx: usize, meta: &PhotoMeta) -> String {
    format!("{plugin_idx}:{}", meta.id)
}

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

fn shuffle_seed_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42)
}

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
            queue.sort_by(|(_, a), (_, b)| b.taken_at.cmp(&a.taken_at));
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
            queue[from..].sort_by(|(_, a), (_, b)| b.taken_at.cmp(&a.taken_at));
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
    fn sync_counts_marks_zero_items_as_exhausted() {
        let mut loader = QueueLoader::new(2);
        loader.sync_counts(&[]);
        assert!(loader.all_exhausted());
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
}
