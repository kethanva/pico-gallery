use picogallery_core::PluginConfig;
use anyhow::{Context, Result};
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
