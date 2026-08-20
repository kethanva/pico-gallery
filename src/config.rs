use anyhow::{Context, Result};
use chrono::{Local, NaiveTime};
use log::warn;
use picogallery_core::{PluginConfig, TargetingAdapter, TargetingState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

// ── Display ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Cut,
    #[default]
    Fade,
    SlideLeft,
    SlideRight,
}

/// Downscale filter used when scaling a decoded photo to the display.
///
/// `Lanczos3` is the sharpest but samples a wide window (~36 taps/pixel) —
/// costly on a Pi Zero. `CatmullRom` (bicubic) is the default: visually close
/// to Lanczos3 for photographic downscales at roughly half the work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResizeFilter {
    /// Fastest, slightly soft.
    Bilinear,
    /// Bicubic middle-ground — default. Near-Lanczos quality, ~half the cost.
    #[default]
    CatmullRom,
    /// Bicubic tuned to reduce ringing/blur.
    Mitchell,
    /// Sharpest, widest sampling window, highest CPU cost.
    Lanczos3,
}

/// Order in which photos are presented.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PhotoOrder {
    /// Random shuffle on each startup (default).
    #[default]
    Shuffle,
    /// Oldest photo first, sorted by EXIF capture date.
    Chronological,
    /// Newest photo first, sorted by EXIF capture date.
    NewestFirst,
    /// Photos grouped into small same-day/same-album runs (max 5), with the
    /// runs themselves shuffled — tells little "stories" instead of pure random.
    DateCluster,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Duration each photo is shown, in seconds.
    #[serde(default = "default_slide_duration")]
    pub slide_duration_secs: u64,

    /// Duration of the transition animation, in milliseconds.
    /// Use 0 to disable (forced Cut on Pi Zero if performance is poor).
    #[serde(default = "default_transition_ms")]
    pub transition_ms: u32,

    #[serde(default)]
    pub transition: Transition,

    /// Fill the screen (may crop) or letterbox (black bars, no crop).
    #[serde(default)]
    pub fill_screen: bool,

    /// Target display width.  0 = auto-detect from SDL2.
    #[serde(default)]
    pub width: u32,

    /// Target display height.  0 = auto-detect from SDL2.
    #[serde(default)]
    pub height: u32,

    /// Frames per second cap.  Lower = less CPU on Pi Zero.
    #[serde(default = "default_fps")]
    pub fps: u32,

    // ── Optional display schedule ────────────────────────────────────────────
    //
    // Both fields must be set to activate scheduling; if either is absent the
    // display is always on (default behaviour).  Times are in 24-hour HH:MM
    // format and interpreted in local time.
    //
    // Example — on 07:00, off 22:00 each day:
    //   on_time  = "07:00"
    //   off_time = "22:00"
    //
    // The schedule is optional and off by default.
    /// Time at which the display turns on each day (HH:MM, local time).
    #[serde(default)]
    pub on_time: Option<String>,

    /// Time at which the display turns off each day (HH:MM, local time).
    #[serde(default)]
    pub off_time: Option<String>,

    /// Order photos are shown: shuffle (default), chronological, newest_first.
    #[serde(default)]
    pub order: PhotoOrder,

    /// Show a metadata pill (album, date, filename) in the bottom-left corner.
    /// Defaults to true; set false to show photos without any overlay.
    #[serde(default = "default_true")]
    pub show_osd: bool,

    /// Draw a small HH:MM clock (local time) at the top centre of each photo.
    /// Updates once per slide — no per-frame cost. Off by default.
    #[serde(default)]
    pub show_clock: bool,

    // ── Memory-safety limits ─────────────────────────────────────────────────
    //
    // Both limits are checked before the expensive decode step and generate a
    // WARN log when tripped — the photo is skipped, not crashed.
    //
    // Recommended values for Pi Zero (512 MB RAM):
    //   max_image_mb   = 20    (raw JPEG file size)
    //   max_megapixels = 12    (decoded pixel count; 12 MP → ~56 MB peak)
    //
    // Leave at 0 to use the built-in defaults (50 MB / 24 MP).
    /// Maximum raw image file size in megabytes.
    /// 0 = use built-in default of 50 MB.
    #[serde(default)]
    pub max_image_mb: u64,

    /// Maximum decoded image size in megapixels (width × height / 1 000 000).
    /// 0 = built-in 24 MP backstop (so an oversized photo can't OOM a 512 MB
    /// Pi Zero 2). Peak RAM ≈ MP × 3 MB for the full-res RGB decode buffer.
    /// Example: 24 MP → ≈72 MB. Set higher to allow 48 MP+ phone photos.
    #[serde(default)]
    pub max_megapixels: u32,

    /// Fill letterbox bars with a blurred, stretched copy of the photo
    /// instead of plain black. Only applies when fill_screen = false.
    #[serde(default = "default_true")]
    pub letterbox_blur: bool,

    /// Downscale filter for the main slide. Lighter filters trade a little
    /// sharpness for materially less CPU per slide on the Pi Zero.
    #[serde(default)]
    pub resize_filter: ResizeFilter,

    /// Slow Ken Burns zoom/pan on each photo. Renders continuously at the
    /// configured fps while a slide is showing (more CPU/GPU load — off by
    /// default; fine on Pi Zero 2, not recommended on the original Pi Zero).
    #[serde(default)]
    pub ken_burns: bool,

    /// Boost photos taken on today's calendar date in previous years by
    /// weaving them near the front of the shuffled queue.
    #[serde(default = "default_true")]
    pub on_this_day_boost: bool,

    // ── Optional night mode ──────────────────────────────────────────────────
    //
    // Between night_start and night_end (HH:MM, local time, may span
    // midnight) photos are dimmed and warm-shifted — easier on the eyes in a
    // dark room. Both times must be set to activate; one cheap pixel pass per
    // slide, no per-frame cost.
    /// Night window start (HH:MM, local). Unset = night mode off.
    #[serde(default)]
    pub night_start: Option<String>,

    /// Night window end (HH:MM, local). Unset = night mode off.
    #[serde(default)]
    pub night_end: Option<String>,

    /// Brightness reduction during the night window, percent (0–90).
    #[serde(default = "default_night_dim")]
    pub night_dim_percent: u8,

    /// Warm tint strength during the night window, percent (0–100).
    /// Reduces blue/green channels to cut harsh cold light.
    #[serde(default = "default_night_warmth")]
    pub night_warmth: u8,

    /// Start in a browsable thumbnail grid (PhotoPrism kiosk style). Click a
    /// photo for fullscreen; Escape or the close control returns to the grid.
    /// Defaults to true so Escape/× return to the gallery instead of quitting.
    #[serde(default = "default_true")]
    pub gallery_mode: bool,

    /// When true, skip photos already shown until every photo in the queue has
    /// been displayed once, then start a fresh cycle.
    #[serde(default)]
    pub no_repeat_shown: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            slide_duration_secs: default_slide_duration(),
            transition_ms: default_transition_ms(),
            transition: Transition::Fade,
            fill_screen: false,
            width: 0,
            height: 0,
            fps: default_fps(),
            on_time: None,
            off_time: None,
            order: PhotoOrder::Shuffle,
            show_osd: true,
            show_clock: false,
            max_image_mb: 0,
            max_megapixels: 0,
            letterbox_blur: true,
            resize_filter: ResizeFilter::CatmullRom,
            ken_burns: false,
            on_this_day_boost: true,
            night_start: None,
            night_end: None,
            night_dim_percent: default_night_dim(),
            night_warmth: default_night_warmth(),
            gallery_mode: true,
            no_repeat_shown: false,
        }
    }
}

impl DisplayConfig {
    /// Returns `true` when the display should be on right now.
    ///
    /// Scheduling is disabled (always on) when:
    /// - neither `on_time` nor `off_time` is set, or
    /// - only one of the two is set (configuration error), or
    /// - either value cannot be parsed as `HH:MM`, or
    /// - both values are identical (zero-width window).
    pub fn schedule_active_now(&self) -> bool {
        let (Some(on_str), Some(off_str)) = (&self.on_time, &self.off_time) else {
            // Scheduling not configured — always on.
            return true;
        };

        let (on, off) = match (
            parse_hhmm(on_str, "on_time"),
            parse_hhmm(off_str, "off_time"),
        ) {
            (Some(a), Some(b)) => (a, b),
            // parse_hhmm has already warned naming the offending field/value.
            _ => return true,
        };

        // start == end is a zero-width window → scheduling disabled, display
        // stays ON. Note the deliberate asymmetry with night_active_now(),
        // where start == end means night mode stays OFF — in both cases the
        // degenerate window resolves to the feature's inert default.
        if on == off {
            return true;
        }

        time_in_window(Local::now().time(), on, off)
    }

    /// Returns `true` when the night dim/warm window is active right now.
    /// Off (always false) unless both `night_start` and `night_end` parse.
    pub fn night_active_now(&self) -> bool {
        let (Some(start_str), Some(end_str)) = (&self.night_start, &self.night_end) else {
            return false;
        };
        let (Some(start), Some(end)) = (
            parse_hhmm(start_str, "night_start"),
            parse_hhmm(end_str, "night_end"),
        ) else {
            // parse_hhmm has already warned naming the offending field/value.
            return false;
        };
        // start == end is a zero-width window → night mode stays OFF.
        // Asymmetric with schedule_active_now() (where start == end keeps the
        // display ON) — both degenerate to the feature's inert default.
        if start == end {
            return false;
        }
        time_in_window(Local::now().time(), start, end)
    }

    /// Returns a human-readable description of the configured schedule, or
    /// `None` if scheduling is disabled.
    pub fn schedule_description(&self) -> Option<String> {
        match (&self.on_time, &self.off_time) {
            (Some(on), Some(off)) => Some(format!("{on} → {off}")),
            _ => None,
        }
    }
}

fn default_slide_duration() -> u64 {
    10
}
fn default_transition_ms() -> u32 {
    800
}
fn default_fps() -> u32 {
    15
}
fn default_true() -> bool {
    true
}
fn default_night_dim() -> u8 {
    25
}
fn default_night_warmth() -> u8 {
    30
}

/// Parse "HH:MM" into a NaiveTime, warning (with the config field name and
/// the offending value) when the input is malformed — config values are user
/// input, and a typo'd time should fail loud rather than silently disable
/// the feature. Out-of-range components ("25:00", "07:60") are rejected by
/// `NaiveTime::from_hms_opt` returning `None`; garbage fails the int parse.
fn parse_hhmm(s: &str, field: &str) -> Option<NaiveTime> {
    let parsed = (|| {
        let mut parts = s.splitn(2, ':');
        let h: u32 = parts.next()?.trim().parse().ok()?;
        let m: u32 = parts.next()?.trim().parse().ok()?;
        NaiveTime::from_hms_opt(h, m, 0)
    })();
    if parsed.is_none() {
        warn!("display.{field}: '{s}' is not a valid HH:MM time — feature disabled");
    }
    parsed
}

/// True when `now` falls inside the half-open `[start, end)` window: the
/// start minute is in, the end minute is out, so `end` of one window and
/// `start` of the next never overlap. Windows may span midnight
/// (start > end → overnight, e.g. 22:00–06:00). Callers handle
/// start == end themselves — see schedule_active_now / night_active_now.
fn time_in_window(now: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

// ── Cache ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Directory for cached images.  Defaults to ~/.cache/picogallery.
    pub dir: Option<PathBuf>,

    /// Maximum cache size in megabytes.
    #[serde(default = "default_cache_mb")]
    pub max_mb: u64,

    /// Number of photos to pre-fetch ahead.
    #[serde(default = "default_prefetch")]
    pub prefetch_count: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_mb: default_cache_mb(),
            prefetch_count: default_prefetch(),
        }
    }
}

fn default_cache_mb() -> u64 {
    256
}
fn default_prefetch() -> usize {
    3
}

impl CacheConfig {
    pub fn resolved_dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("picogallery")
        })
    }
}

// ── Auth (headless OAuth / device-code) ──────────────────────────────────────

/// Bounds for plugin sign-in at startup. A plugin that sits in
/// `PendingUserAction` longer than `pending_timeout_secs` is disabled so the
/// frame can still start from other sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Wall-clock a plugin may sit in `PendingUserAction` at startup before
    /// the source is dropped. 0 = wait forever (only sensible with a console).
    #[serde(default = "default_auth_pending_timeout_secs")]
    pub pending_timeout_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            pending_timeout_secs: default_auth_pending_timeout_secs(),
        }
    }
}

fn default_auth_pending_timeout_secs() -> u64 {
    180
}

/// Peak RSS estimate for the decode/prefetch/gallery working set, in MiB.
///
/// `max_image_mb == 0` still hard-caps fetches at 50 MB; the estimator uses
/// 20 MB (the documented Pi Zero recommendation) so stock auto-4K + gallery
/// still boots. Gallery thumbs are counted only when `gallery_mode` is on.
pub(crate) fn estimated_peak_image_mb(
    source_mp: u32,
    budget_w: u64,
    budget_h: u64,
    prefetch_count: usize,
    max_image_mb: u64,
    gallery_mode: bool,
) -> u64 {
    let frame_mb = (budget_w
        .saturating_mul(budget_h)
        .saturating_mul(4)
        .saturating_add(1_048_575))
        / 1_048_576;
    let decode_peak_mb = u64::from(source_mp) * 3;
    let inflight_mb = if max_image_mb == 0 { 20 } else { max_image_mb };
    let thumb_cache_mb = if gallery_mode {
        let cell = u64::from(crate::gallery::cell_px_for_width(budget_w as u32).max(1));
        (crate::gallery::THUMB_CACHE_CAP as u64)
            .saturating_mul(cell)
            .saturating_mul(cell)
            .saturating_mul(4)
            .saturating_add(1_048_575)
            / 1_048_576
    } else {
        0
    };
    let ring_mb = frame_mb.saturating_mul(prefetch_count as u64 + 3);
    decode_peak_mb
        .saturating_add(inflight_mb)
        .saturating_add(thumb_cache_mb)
        .saturating_add(ring_mb)
        .saturating_add(32)
}

// ── Web remote ───────────────────────────────────────────────────────────────

/// Built-in HTTP remote control: a phone-friendly page with next / prev /
/// pause buttons plus a tiny JSON status API. Near-zero cost while idle.
///
/// A bearer token is required when enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Enable the HTTP remote (default: false).
    #[serde(default)]
    pub enabled: bool,

    /// TCP port to listen on.
    #[serde(default = "default_remote_port")]
    pub port: u16,

    /// Bind address. Defaults to loopback; a token is required on every API call.
    #[serde(default = "default_remote_bind")]
    pub bind: String,

    /// Path to a file containing the remote-control bearer token. Prefer this
    /// over environment variables so systemd can use LoadCredential.
    #[serde(default)]
    pub token_file: Option<String>,

    /// Resolved from `token_file` or `PICOGALLERY_REMOTE_TOKEN`; never
    /// serialized back to config.toml.
    #[serde(skip)]
    pub token: Option<String>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_remote_port(),
            bind: default_remote_bind(),
            token_file: None,
            token: None,
        }
    }
}

fn default_remote_port() -> u16 {
    8188
}
fn default_remote_bind() -> String {
    "127.0.0.1".to_string()
}

// ── HDMI CEC remote ──────────────────────────────────────────────────────────

/// HDMI CEC input from a TV remote (Linux only).
///
/// Maps common transport keys to slideshow controls without opening a network
/// port. Requires a CEC adapter exposed by the kernel (usually `/dev/cec0`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CecConfig {
    /// Enable HDMI CEC remote control input.
    #[serde(default)]
    pub enabled: bool,

    /// Linux CEC character device.
    #[serde(default = "default_cec_device")]
    pub device: String,

    /// Poll interval in milliseconds for incoming CEC messages.
    #[serde(default = "default_cec_poll_ms")]
    pub poll_ms: u64,
}

impl Default for CecConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: default_cec_device(),
            poll_ms: default_cec_poll_ms(),
        }
    }
}

fn default_cec_device() -> String {
    "/dev/cec0".to_string()
}
fn default_cec_poll_ms() -> u64 {
    250
}

// ── Wi-Fi ──────────────────────────────────────────────────────────────────────

/// Optional Wi-Fi credentials the app can apply to the host OS.
///
/// Only effective on Linux/Raspberry Pi when the service account is authorized
/// by NetworkManager/udisks or a local privileged helper. Standard appliance
/// installs run unprivileged, so host Wi-Fi should normally be provisioned by
/// the OS rather than this optional UI feature.
/// `password` is the WPA2 pre-shared key — WPA-Enterprise (username/identity)
/// is not supported. Credentials live here so the on-screen settings menu can
/// edit them; treat the config file as sensitive (it may hold the passphrase).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WifiConfig {
    /// Apply these Wi-Fi settings at startup and when changed via the menu.
    #[serde(default)]
    pub enabled: bool,

    /// Network name (SSID).
    #[serde(default)]
    pub ssid: String,

    /// WPA2 pre-shared key (passphrase). Prefer `password_file` or
    /// `PICOGALLERY_WIFI_PASSWORD` so the passphrase is not stored in TOML.
    #[serde(default)]
    pub password: String,

    /// Path to a file whose contents are the WPA2 passphrase (trimmed).
    /// When set, overrides `password` at load time.
    #[serde(default)]
    pub password_file: Option<String>,

    /// ISO 3166 alpha-2 country code (e.g. "US", "GB"). Some regulatory setups
    /// require it for `wpa_supplicant`; ignored by the `nmcli` backend.
    #[serde(default)]
    pub country: String,
}

// ── Plugins ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Must match `PhotoPlugin::name()`.
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(flatten)]
    pub config: PluginConfig,
}

// ── Targeting (album / favourites filters) ───────────────────────────────────

/// Album and favourites filters applied to the active photo source at startup
/// and when changed from the settings menu. Mapped into each plugin's config
/// via [`TargetingAdapter`] from `PhotoPlugin::capabilities()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetingConfig {
    /// PhotoPrism album slug/UID or directory sub-folder name. Empty = all albums.
    #[serde(default)]
    pub album: String,

    /// When true, only show favourited photos (PhotoPrism). Ignored by directory.
    #[serde(default)]
    pub favorites_only: bool,
}

impl TargetingConfig {
    /// Human label for the settings menu.
    pub fn album_label(&self) -> &str {
        if self.album.is_empty() {
            "all albums"
        } else {
            self.album.as_str()
        }
    }
}

// ── Root config ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct SecretOrigins {
    pub wifi_from_external: bool,
    pub remote_token_external: bool,
    pub plugin_external: std::collections::HashSet<(String, String)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub secret_origins: SecretOrigins,

    #[serde(default)]
    pub display: DisplayConfig,

    #[serde(default)]
    pub cache: CacheConfig,

    /// Headless OAuth / device-code sign-in bounds.
    #[serde(default)]
    pub auth: AuthConfig,

    /// Optional HTTP remote control.
    #[serde(default)]
    pub remote: RemoteConfig,

    /// Optional HDMI CEC TV-remote input (Linux only).
    #[serde(default)]
    pub cec: CecConfig,

    /// Optional Wi-Fi credentials applied to the host OS (Linux/Pi only).
    #[serde(default)]
    pub wifi: WifiConfig,

    /// Album / favourites targeting for the active photo source.
    #[serde(default)]
    pub targeting: TargetingConfig,

    /// One entry per plugin.  Order determines display order when mixing sources.
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,

    /// Extra top-level keys are silently ignored.
    #[serde(flatten)]
    pub _extra: HashMap<String, toml::Value>,
}

impl Config {
    /// Load from `path` (TOML).
    pub fn from_file(path: &Path) -> Result<Self> {
        // Guard against huge / maliciously crafted config files before parsing.
        const MAX_CONFIG_BYTES: u64 = 1024 * 1024; // 1 MB
        let file_size = std::fs::metadata(path)
            .with_context(|| format!("stat config {}", path.display()))?
            .len();
        if file_size > MAX_CONFIG_BYTES {
            return Err(anyhow::anyhow!(
                "config file is too large ({} KB) — max 1 MB",
                file_size / 1024
            ));
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config
            .apply_secret_overrides()
            .with_context(|| format!("resolving secrets for {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validating config {}", path.display()))?;
        Self::restrict_private_permissions(path);
        Ok(config)
    }

    /// Resolve file-backed and environment secrets into in-memory config.
    ///
    /// Plugin keys: `password`, `app_password`, `client_secret` via
    /// `{key}_file` and `PICOGALLERY_{PLUGIN}_{KEY}`.
    /// Wi-Fi: `password_file` and `PICOGALLERY_WIFI_PASSWORD`.
    /// Remote: `token_file` and `PICOGALLERY_REMOTE_TOKEN`.
    pub fn apply_secret_overrides(&mut self) -> Result<()> {
        const PLUGIN_SECRETS: &[&str] = &["password", "app_password", "client_secret"];

        if let Some(path) = self.wifi.password_file.clone() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading wifi.password_file ({path})"))?;
            let value = raw.trim().to_string();
            if value.is_empty() {
                return Err(anyhow::anyhow!("wifi.password_file ({path}) is empty"));
            }
            self.wifi.password = value;
        }
        match std::env::var("PICOGALLERY_WIFI_PASSWORD") {
            Ok(v) if !v.is_empty() => {
                self.wifi.password = v;
                self.secret_origins.wifi_from_external = true;
            }
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "PICOGALLERY_WIFI_PASSWORD is set but empty"
                ));
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(e) => return Err(anyhow::anyhow!("reading PICOGALLERY_WIFI_PASSWORD: {e}")),
        }

        if let Some(path) = self.remote.token_file.clone() {
            let value = std::fs::read_to_string(&path)
                .with_context(|| format!("reading remote.token_file ({path})"))?
                .trim()
                .to_string();
            if value.is_empty() {
                return Err(anyhow::anyhow!("remote.token_file ({path}) is empty"));
            }
            self.remote.token = Some(value);
        }
        match std::env::var("PICOGALLERY_REMOTE_TOKEN") {
            Ok(v) if !v.is_empty() => {
                self.remote.token = Some(v);
                self.secret_origins.remote_token_external = true;
            }
            Ok(_) => return Err(anyhow::anyhow!("PICOGALLERY_REMOTE_TOKEN is set but empty")),
            Err(std::env::VarError::NotPresent) => {}
            Err(e) => return Err(anyhow::anyhow!("reading PICOGALLERY_REMOTE_TOKEN: {e}")),
        }

        for entry in &mut self.plugins {
            for key in PLUGIN_SECRETS {
                entry.config.resolve_secret_file(key)?;
                if entry.config.apply_env_secret(&entry.name, key)? {
                    self.secret_origins
                        .plugin_external
                        .insert((entry.name.clone(), key.to_string()));
                }
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=3600).contains(&self.display.slide_duration_secs) {
            return Err(anyhow::anyhow!(
                "display.slide_duration_secs must be between 1 and 3600"
            ));
        }
        if self.display.transition_ms > 10_000 {
            return Err(anyhow::anyhow!(
                "display.transition_ms must not exceed 10000"
            ));
        }
        if !(1..=60).contains(&self.display.fps) {
            return Err(anyhow::anyhow!("display.fps must be between 1 and 60"));
        }
        let dimensions_are_auto = self.display.width == 0 && self.display.height == 0;
        let dimensions_are_valid = (320..=8192).contains(&self.display.width)
            && (240..=8192).contains(&self.display.height);
        if !dimensions_are_auto && !dimensions_are_valid {
            return Err(anyhow::anyhow!(
                "display width/height must both be auto (0) or within 320x240 to 8192x8192"
            ));
        }
        if self.display.max_image_mb > 50 || self.display.max_megapixels > 32 {
            return Err(anyhow::anyhow!(
                "display image limits exceed the supported appliance resource budget"
            ));
        }
        if self.display.night_dim_percent > 90 || self.display.night_warmth > 100 {
            return Err(anyhow::anyhow!(
                "display night percentages are out of range"
            ));
        }
        if self.cache.max_mb == 0 || self.cache.max_mb > 4096 {
            return Err(anyhow::anyhow!("cache.max_mb must be between 1 and 4096"));
        }
        if self.cache.prefetch_count == 0 || self.cache.prefetch_count > 8 {
            return Err(anyhow::anyhow!(
                "cache.prefetch_count must be between 1 and 8"
            ));
        }
        let source_mp = if self.display.max_megapixels == 0 {
            24
        } else {
            self.display.max_megapixels
        };
        // Only the image currently being decoded has a source-resolution RGB
        // buffer. Prefetched entries are cropped/scaled RGBA frames, so the
        // old `MP * 3 * (1 + prefetch)` formula over-counted every slot as a
        // full camera image and rejected safe 1080p configurations. Include
        // current/menu/resize transients plus 32 MiB of process headroom.
        // Auto resolution assumes a conservative 4K panel; an explicit 1080p
        // setting can safely use a deeper prefetch ring.
        //
        // Gallery thumbs (up to THUMB_CACHE_CAP cells) and one in-flight
        // compressed JPEG were missing from the original estimate. Default
        // `max_image_mb = 0` still hard-caps fetches at 50 MB, but the
        // estimator uses 20 MB (the documented Pi Zero recommendation) so
        // stock auto-4K + gallery + prefetch 3 still starts. An explicit
        // `max_image_mb = 50` at 4K is rejected — that combination really
        // does not fit in 384 MiB.
        let (budget_w, budget_h) = if dimensions_are_auto {
            (3840u64, 2160u64)
        } else {
            (self.display.width as u64, self.display.height as u64)
        };
        let footprint = estimated_peak_image_mb(
            source_mp,
            budget_w,
            budget_h,
            self.cache.prefetch_count,
            self.display.max_image_mb,
            self.display.gallery_mode,
        );
        if footprint > 350 {
            return Err(anyhow::anyhow!(
                "max_megapixels ({source_mp}), display budget ({budget_w}x{budget_h}), prefetch_count ({}), max_image_mb ({}), gallery_mode ({}) result in an estimated peak image footprint ({footprint} MB) exceeding the memory limit",
                self.cache.prefetch_count,
                self.display.max_image_mb,
                self.display.gallery_mode
            ));
        }
        if self.auth.pending_timeout_secs != 0
            && !(10..=3600).contains(&self.auth.pending_timeout_secs)
        {
            return Err(anyhow::anyhow!(
                "auth.pending_timeout_secs must be 0 (wait forever) or between 10 and 3600"
            ));
        }
        if self.remote.bind.trim().is_empty() {
            return Err(anyhow::anyhow!("remote.bind must not be empty"));
        }
        self.remote
            .bind
            .parse::<IpAddr>()
            .with_context(|| "remote.bind must be a literal IPv4 or IPv6 address")?;
        if self.remote.enabled && self.remote.port == 0 {
            return Err(anyhow::anyhow!("remote.port must not be zero"));
        }
        if self.remote.enabled && self.remote.token.as_deref().is_none_or(str::is_empty) {
            return Err(anyhow::anyhow!(
                "remote.enabled requires remote.token_file or PICOGALLERY_REMOTE_TOKEN"
            ));
        }
        if self.remote.enabled
            && self
                .remote
                .token
                .as_deref()
                .is_some_and(|token| token.len() < 16)
        {
            return Err(anyhow::anyhow!(
                "remote token must contain at least 16 characters"
            ));
        }
        if self.remote.enabled
            && self
                .remote
                .token
                .as_deref()
                .is_some_and(|token| token.chars().any(char::is_control))
        {
            return Err(anyhow::anyhow!(
                "remote token must not contain control characters"
            ));
        }

        if self.cec.enabled && !(50..=5000).contains(&self.cec.poll_ms) {
            return Err(anyhow::anyhow!("cec.poll_ms must be between 50 and 5000"));
        }
        Ok(())
    }

    /// Before writing config.toml, drop inline secret values that are backed
    /// by a `*_file` path so Save does not re-embed file contents into TOML.
    pub fn redact_secrets_for_persistence(&mut self) {
        const PLUGIN_SECRETS: &[&str] = &["password", "app_password", "client_secret"];

        let has_wifi_file = self
            .wifi
            .password_file
            .as_ref()
            .is_some_and(|p| !p.is_empty());
        if has_wifi_file || self.secret_origins.wifi_from_external {
            self.wifi.password.clear();
        }

        if self.secret_origins.remote_token_external {
            self.remote.token = None;
        }

        for entry in &mut self.plugins {
            for key in PLUGIN_SECRETS {
                let file_key = format!("{key}_file");
                if entry.config.get_str(&file_key).is_some()
                    || self
                        .secret_origins
                        .plugin_external
                        .contains(&(entry.name.clone(), key.to_string()))
                {
                    entry.config.values.remove(*key);
                }
            }
        }
    }

    /// Best-effort: owner-only permissions on the config file (0600) and its
    /// parent directory (0700).  The file may hold Wi-Fi and PhotoPrism
    /// passwords in plain text — tighten perms on load so older installs that
    /// were created world-readable are fixed at startup.
    pub fn restrict_private_permissions(path: &Path) {
        #[cfg(unix)]
        {
            use log::warn;
            use std::os::unix::fs::PermissionsExt;
            if path == Config::default_path() {
                if let Some(parent) = path.parent() {
                    if let Err(e) =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    {
                        warn!(
                            "Could not restrict config dir {} to 0700: {}",
                            parent.display(),
                            e
                        );
                    }
                }
            }
            if path.exists() {
                if let Err(e) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                {
                    warn!(
                        "Could not restrict config {} to 0600: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    /// Default config file path: `~/.config/picogallery/config.toml`.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("picogallery")
            .join("config.toml")
    }

    /// Ensure directories exist. Cache-dir failure is non-fatal: the engine
    /// degrades to [`crate::cache::CacheHandle`] disabled rather than refusing
    /// to start (spec §5). Config-dir creation stays with `--generate-config`.
    pub fn ensure_dirs(&self) -> Result<()> {
        let dir = self.cache.resolved_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(
                "Could not create cache dir {}: {e} — running without disk cache",
                dir.display()
            );
        }
        Ok(())
    }

    /// Return config for plugin named `name`, if enabled.
    pub fn plugin_config(&self, name: &str) -> Option<&PluginConfig> {
        self.plugins
            .iter()
            .find(|p| p.name == name && p.enabled)
            .map(|p| &p.config)
    }

    /// Copy `[targeting]` into enabled plugin configs using each plugin's
    /// [`TargetingAdapter`]. Adapters come from `PhotoPlugin::capabilities()`
    /// so the engine never hard-codes plugin names.
    pub fn apply_targeting(&mut self, adapters: &[(&str, TargetingAdapter)]) {
        let state = TargetingState {
            album: self.targeting.album.clone(),
            favorites_only: self.targeting.favorites_only,
        };
        for entry in &mut self.plugins {
            if !entry.enabled {
                continue;
            }
            let Some((_, adapter)) = adapters.iter().find(|(n, _)| *n == entry.name) else {
                continue;
            };
            if *adapter == TargetingAdapter::NONE {
                continue;
            }
            adapter.apply(&state, &mut entry.config);
            // PhotoPrism historically also accepted `albums`; clear it when
            // writing the singular `album` key so filters don't fight.
            if adapter.album_key == Some("album") {
                entry.config.values.remove("albums");
            }
        }
    }

    /// Read album / favourites filter back from the first enabled plugin that
    /// advertises a targeting adapter.
    pub fn sync_targeting_from_plugins(&mut self, adapters: &[(&str, TargetingAdapter)]) {
        for entry in &self.plugins {
            if !entry.enabled {
                continue;
            }
            let Some((_, adapter)) = adapters.iter().find(|(n, _)| *n == entry.name) else {
                continue;
            };
            if *adapter == TargetingAdapter::NONE {
                continue;
            }
            let read = adapter.read(&entry.config);
            if self.targeting.album.is_empty() {
                self.targeting.album = read.album;
            }
            if !self.targeting.favorites_only {
                self.targeting.favorites_only = read.favorites_only;
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    // ── resize_filter: lighter default + config override + back-compat ──────

    #[test]
    fn resize_filter_defaults_to_catmull_rom() {
        assert_eq!(
            DisplayConfig::default().resize_filter,
            ResizeFilter::CatmullRom
        );
    }

    #[test]
    fn resize_filter_parses_snake_case_override() {
        let cfg: DisplayConfig =
            toml::from_str("resize_filter = \"lanczos3\"").expect("parse resize_filter");
        assert_eq!(cfg.resize_filter, ResizeFilter::Lanczos3);
    }

    #[test]
    fn resize_filter_absent_falls_back_to_default() {
        // Existing configs written before this field must still deserialize.
        let cfg: DisplayConfig = toml::from_str("fps = 15").expect("parse without resize_filter");
        assert_eq!(cfg.resize_filter, ResizeFilter::CatmullRom);
    }

    // ── time_in_window: overnight wrap (22:00–06:00) ────────────────────────

    #[test]
    fn overnight_window_excludes_end_boundary() {
        // Half-open [start, end): 06:00 itself is already outside.
        assert!(!time_in_window(t(6, 0), t(22, 0), t(6, 0)));
    }

    #[test]
    fn overnight_window_includes_start_boundary() {
        assert!(time_in_window(t(22, 0), t(22, 0), t(6, 0)));
    }

    #[test]
    fn overnight_window_includes_early_morning() {
        assert!(time_in_window(t(2, 0), t(22, 0), t(6, 0)));
    }

    #[test]
    fn overnight_window_excludes_midday() {
        assert!(!time_in_window(t(12, 0), t(22, 0), t(6, 0)));
    }

    // ── time_in_window: same-day window ─────────────────────────────────────

    #[test]
    fn daytime_window_is_half_open() {
        assert!(time_in_window(t(7, 0), t(7, 0), t(22, 0))); // start in
        assert!(!time_in_window(t(22, 0), t(7, 0), t(22, 0))); // end out
        assert!(!time_in_window(t(6, 59), t(7, 0), t(22, 0)));
        assert!(time_in_window(t(21, 59), t(7, 0), t(22, 0)));
    }

    // ── start == end asymmetry between the two window features ──────────────

    #[test]
    fn equal_on_off_times_keep_display_always_on() {
        let cfg = DisplayConfig {
            on_time: Some("08:00".into()),
            off_time: Some("08:00".into()),
            ..Default::default()
        };
        assert!(cfg.schedule_active_now());
    }

    #[test]
    fn equal_night_times_keep_night_mode_off() {
        let cfg = DisplayConfig {
            night_start: Some("08:00".into()),
            night_end: Some("08:00".into()),
            ..Default::default()
        };
        assert!(!cfg.night_active_now());
    }

    // ── parse_hhmm: malformed inputs ─────────────────────────────────────────

    #[test]
    fn parse_hhmm_accepts_valid_times() {
        assert_eq!(parse_hhmm("07:00", "test"), Some(t(7, 0)));
        assert_eq!(parse_hhmm("00:00", "test"), Some(t(0, 0)));
        assert_eq!(parse_hhmm("23:59", "test"), Some(t(23, 59)));
        assert_eq!(parse_hhmm(" 7 : 5 ", "test"), Some(t(7, 5))); // trimmed
    }

    #[test]
    fn parse_hhmm_rejects_out_of_range_components() {
        assert_eq!(parse_hhmm("07:60", "test"), None); // minute 60
        assert_eq!(parse_hhmm("25:00", "test"), None); // hour 25
        assert_eq!(parse_hhmm("24:00", "test"), None); // hour 24
    }

    #[test]
    fn parse_hhmm_rejects_garbage() {
        assert_eq!(parse_hhmm("garbage", "test"), None);
        assert_eq!(parse_hhmm("", "test"), None);
        assert_eq!(parse_hhmm("07", "test"), None); // no minutes
        assert_eq!(parse_hhmm("-1:30", "test"), None); // negative hour
        assert_eq!(parse_hhmm("07:xx", "test"), None);
    }

    #[test]
    fn malformed_schedule_times_disable_scheduling_display_stays_on() {
        let cfg = DisplayConfig {
            on_time: Some("25:00".into()),
            off_time: Some("22:00".into()),
            ..Default::default()
        };
        assert!(cfg.schedule_active_now());
    }

    #[test]
    fn malformed_night_times_disable_night_mode() {
        let cfg = DisplayConfig {
            night_start: Some("22:xx".into()),
            night_end: Some("06:00".into()),
            ..Default::default()
        };
        assert!(!cfg.night_active_now());
    }

    #[test]
    fn apply_targeting_syncs_photoprism_plugin() {
        let mut cfg = Config {
            plugins: vec![PluginEntry {
                name: "photoprism".into(),
                enabled: true,
                config: PluginConfig::default(),
            }],
            ..Default::default()
        };
        cfg.targeting.album = "trip-2024".into();
        cfg.targeting.favorites_only = true;
        let adapters = [(
            "photoprism",
            TargetingAdapter {
                album_key: Some("album"),
                album_as_array: false,
                favorites_key: Some("favorites"),
            },
        )];
        cfg.apply_targeting(&adapters);
        let pp = cfg.plugins.iter().find(|p| p.name == "photoprism").unwrap();
        assert_eq!(pp.config.get_str("album"), Some("trip-2024"));
        assert_eq!(
            pp.config.values.get("favorites").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn sync_targeting_reads_plugin_when_targeting_empty() {
        let mut cfg = Config::default();
        let mut pc = PluginConfig::default();
        pc.values
            .insert("album".into(), serde_json::Value::String("family".into()));
        cfg.plugins = vec![PluginEntry {
            name: "photoprism".into(),
            enabled: true,
            config: pc,
        }];
        let adapters = [(
            "photoprism",
            TargetingAdapter {
                album_key: Some("album"),
                album_as_array: false,
                favorites_key: Some("favorites"),
            },
        )];
        cfg.sync_targeting_from_plugins(&adapters);
        assert_eq!(cfg.targeting.album, "family");
    }

    #[test]
    fn secret_file_overrides_plugin_password() {
        let dir = tempfile_dir();
        let secret_path = dir.join("pp.pass");
        std::fs::write(&secret_path, "from-file\n").unwrap();

        let mut cfg = Config::default();
        let mut pc = PluginConfig::default();
        pc.values
            .insert("password".into(), serde_json::json!("inline"));
        pc.values.insert(
            "password_file".into(),
            serde_json::json!(secret_path.to_str().unwrap()),
        );
        cfg.plugins.push(PluginEntry {
            name: "photoprism".into(),
            enabled: true,
            config: pc,
        });
        cfg.apply_secret_overrides().unwrap();
        assert_eq!(cfg.plugins[0].config.get_str("password"), Some("from-file"));
        assert_eq!(
            cfg.plugins[0].config.get_str("password_file"),
            Some(secret_path.to_str().unwrap())
        );
    }

    #[test]
    fn env_overrides_wifi_password() {
        let mut cfg = Config::default();
        cfg.wifi.password = "old".into();
        std::env::set_var("PICOGALLERY_WIFI_PASSWORD", "from-env");
        cfg.apply_secret_overrides().unwrap();
        std::env::remove_var("PICOGALLERY_WIFI_PASSWORD");
        assert_eq!(cfg.wifi.password, "from-env");
    }

    #[test]
    fn rejects_unbounded_prefetch_configuration() {
        let mut cfg = Config::default();
        cfg.cache.prefetch_count = 10_000;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("prefetch_count"));
    }

    #[test]
    fn image_budget_counts_prefetch_as_display_sized_frames() {
        let mut cfg = Config::default();
        cfg.display.width = 1920;
        cfg.display.height = 1080;
        cfg.cache.prefetch_count = 8;
        assert!(cfg.validate().is_ok());

        // At an explicit 8K resolution even one prefetched RGBA frame exceeds
        // the appliance's 384 MiB service envelope once decode transients are
        // included.
        cfg.display.width = 8192;
        cfg.display.height = 8192;
        cfg.cache.prefetch_count = 1;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("peak image footprint"));
    }

    #[test]
    fn estimator_still_accepts_pi_zero_baseline() {
        let mut cfg = Config::default();
        cfg.display.width = 1920;
        cfg.display.height = 1080;
        cfg.cache.prefetch_count = 3;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn estimator_counts_gallery_thumbs_and_inflight_jpeg() {
        // Old formula (decode + ring + 32) accepted explicit 4K + prefetch 3
        // at ~296 MB. Counting gallery thumbs + a 50 MB JPEG pushes it over.
        let mut cfg = Config::default();
        cfg.display.width = 3840;
        cfg.display.height = 2160;
        cfg.cache.prefetch_count = 3;
        cfg.display.max_megapixels = 24;
        cfg.display.max_image_mb = 50;
        cfg.display.gallery_mode = true;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("peak image footprint"),
            "expected 4K+gallery+50MB JPEG to exceed 350 MB, got: {err}"
        );
        assert!(err.contains("gallery_mode"));
        assert!(err.contains("max_image_mb"));
    }

    #[test]
    fn auth_pending_timeout_defaults_to_180() {
        assert_eq!(AuthConfig::default().pending_timeout_secs, 180);
        let mut cfg = Config::default();
        cfg.auth.pending_timeout_secs = 5;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("pending_timeout_secs"));
        cfg.auth.pending_timeout_secs = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn installed_unit_matches_install_path() {
        let unit = include_str!("../picogallery.service");
        let exec: Vec<&str> = unit
            .lines()
            .filter(|l| l.starts_with("ExecStart="))
            .collect();
        assert_eq!(exec, ["ExecStart=/usr/bin/picogallery"]);
        assert!(unit.contains("ReadWritePaths=-"));
        assert!(unit.contains("MemoryMax=384M"));
        assert!(unit.contains("TasksMax=128"));
        let install = include_str!("../install.sh");
        assert!(
            !install.contains("[Service]"),
            "install.sh must not author a second unit; render from picogallery.service"
        );
    }

    #[test]
    fn remote_defaults_to_loopback() {
        assert_eq!(RemoteConfig::default().bind, "127.0.0.1");
    }

    #[test]
    fn enabled_remote_requires_a_token() {
        let mut cfg = Config::default();
        cfg.remote.enabled = true;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("REMOTE_TOKEN"));
    }

    #[test]
    fn rejects_out_of_range_display_budget() {
        let mut cfg = Config::default();
        cfg.display.fps = 120;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("display.fps"));
    }

    #[test]
    fn rejects_image_budget_that_exceeds_service_memory_limit() {
        let mut cfg = Config::default();
        cfg.display.max_megapixels = 33;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("resource budget"));
    }

    #[test]
    fn rejects_non_ip_remote_bind_and_control_char_token() {
        let mut cfg = Config::default();
        cfg.remote.bind = "photos.local".into();
        assert!(cfg.validate().is_err());

        cfg.remote.bind = "127.0.0.1".into();
        cfg.remote.enabled = true;
        cfg.remote.token = Some("this-token-has-a-newline\n".into());
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("control characters"));
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("picogallery-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn redact_clears_inline_when_file_set() {
        let mut cfg = Config::default();
        cfg.wifi.password = "secret".into();
        cfg.wifi.password_file = Some("/run/wifi.pass".into());
        let mut pc = PluginConfig::default();
        pc.values
            .insert("password".into(), serde_json::json!("inline"));
        pc.values
            .insert("password_file".into(), serde_json::json!("/run/pp.pass"));
        cfg.plugins.push(PluginEntry {
            name: "photoprism".into(),
            enabled: true,
            config: pc,
        });
        cfg.redact_secrets_for_persistence();
        assert!(cfg.wifi.password.is_empty());
        assert!(cfg.plugins[0].config.get_str("password").is_none());
        assert_eq!(
            cfg.plugins[0].config.get_str("password_file"),
            Some("/run/pp.pass")
        );
    }

    #[test]
    fn config_validation_expanded_edge_cases() {
        // slide_duration_secs
        let mut cfg = Config::default();
        cfg.display.slide_duration_secs = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("slide_duration_secs"));
        cfg.display.slide_duration_secs = 3601;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("slide_duration_secs"));

        // transition_ms
        cfg = Config::default();
        cfg.display.transition_ms = 10_001;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("transition_ms"));

        // width and height
        cfg = Config::default();
        cfg.display.width = 1920; // One set, one 0
        cfg.display.height = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("width/height must both be auto"));
        cfg.display.width = 100; // Out of bounds
        cfg.display.height = 100;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("within 320x240"));

        // image size limits
        cfg = Config::default();
        cfg.display.max_image_mb = 51;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("image limits exceed"));

        // night mode percentages
        cfg = Config::default();
        cfg.display.night_dim_percent = 91;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("night percentages"));
        cfg.display.night_dim_percent = 50;
        cfg.display.night_warmth = 101;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("night percentages"));

        // cache limits
        cfg = Config::default();
        cfg.cache.max_mb = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cache.max_mb"));
        cfg.cache.max_mb = 5000;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cache.max_mb"));

        cfg = Config::default();
        cfg.cache.prefetch_count = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cache.prefetch_count"));

        // remote port
        cfg = Config::default();
        cfg.remote.enabled = true;
        cfg.remote.token = Some("1234567890123456".to_string());
        cfg.remote.port = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("remote.port"));

        // cec poll
        cfg = Config::default();
        cfg.cec.enabled = true;
        cfg.cec.poll_ms = 49;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cec.poll_ms"));
        cfg.cec.poll_ms = 5001;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cec.poll_ms"));
    }
}
