use picogallery_core::PluginConfig;
use anyhow::{Context, Result};
use chrono::{Local, NaiveTime};
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── Display ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Cut,
    Fade,
    SlideLeft,
    SlideRight,
}

impl Default for Transition {
    fn default() -> Self { Self::Fade }
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
            on_time:  None,
            off_time: None,
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

        let parse = |s: &str| -> Option<NaiveTime> {
            let mut parts = s.splitn(2, ':');
            let h: u32 = parts.next()?.parse().ok()?;
            let m: u32 = parts.next()?.parse().ok()?;
            NaiveTime::from_hms_opt(h, m, 0)
        };

        let (on, off) = match (parse(on_str), parse(off_str)) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                warn!(
                    "display.on_time/off_time could not be parsed as HH:MM \
                     (got '{}' / '{}') — scheduling disabled",
                    on_str, off_str
                );
                return true;
            }
        };

        if on == off {
            return true; // zero-width window → treat as disabled
        }

        let now = Local::now().time();

        if on < off {
            // Normal window: active between on_time and off_time on the same day.
            now >= on && now < off
        } else {
            // Overnight window (e.g., on=22:00 off=06:00): active if past on OR before off.
            now >= on || now < off
        }
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

fn default_slide_duration() -> u64 { 10 }
fn default_transition_ms() -> u32  { 800 }
fn default_fps() -> u32            { 15 }

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

fn default_cache_mb()  -> u64   { 256 }
fn default_prefetch()  -> usize { 3   }

impl CacheConfig {
    pub fn resolved_dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("picogallery")
        })
    }
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
        toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))
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
        std::fs::create_dir_all(self.cache.resolved_dir())
            .context("creating cache dir")?;
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
