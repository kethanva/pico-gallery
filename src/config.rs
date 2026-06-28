use anyhow::{Context, Result};
use chrono::{Local, NaiveTime};
use log::warn;
use picogallery_core::PluginConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    // Leave at 0 to use the built-in defaults (50 MB / no MP limit).
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
            ken_burns: false,
            on_this_day_boost: true,
            night_start: None,
            night_end: None,
            night_dim_percent: default_night_dim(),
            night_warmth: default_night_warmth(),
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

// ── Web remote ───────────────────────────────────────────────────────────────

/// Built-in HTTP remote control: a phone-friendly page with next / prev /
/// pause buttons plus a tiny JSON status API. Near-zero cost while idle.
///
/// NOTE: there is no authentication — only enable on a trusted LAN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Enable the HTTP remote (default: false).
    #[serde(default)]
    pub enabled: bool,

    /// TCP port to listen on.
    #[serde(default = "default_remote_port")]
    pub port: u16,

    /// Bind address. Default "0.0.0.0" (all interfaces); use "127.0.0.1"
    /// to restrict to local-only access.
    #[serde(default = "default_remote_bind")]
    pub bind: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_remote_port(),
            bind: default_remote_bind(),
        }
    }
}

fn default_remote_port() -> u16 {
    8188
}
fn default_remote_bind() -> String {
    "0.0.0.0".to_string()
}

// ── Wi-Fi ──────────────────────────────────────────────────────────────────────

/// Optional Wi-Fi credentials the app can apply to the host OS.
///
/// Only effective on Linux/Raspberry Pi: applying it (re)writes the system
/// Wi-Fi configuration and reconnects, which needs root (the Pi appliance
/// service runs as root). On other platforms applying is a logged no-op error.
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

    /// WPA2 pre-shared key (passphrase).
    #[serde(default)]
    pub password: String,

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

// ── Root config ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub display: DisplayConfig,

    #[serde(default)]
    pub cache: CacheConfig,

    /// Optional HTTP remote control.
    #[serde(default)]
    pub remote: RemoteConfig,

    /// Optional Wi-Fi credentials applied to the host OS (Linux/Pi only).
    #[serde(default)]
    pub wifi: WifiConfig,

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
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Default config file path: `~/.config/picogallery/config.toml`.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("picogallery")
            .join("config.toml")
    }

    /// Ensure all required directories exist.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.cache.resolved_dir()).context("creating cache dir")?;
        Ok(())
    }

    /// Return config for plugin named `name`, if enabled.
    pub fn plugin_config(&self, name: &str) -> Option<&PluginConfig> {
        self.plugins
            .iter()
            .find(|p| p.name == name && p.enabled)
            .map(|p| &p.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
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
}
