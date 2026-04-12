/// Core plugin abstraction for PicoGallery.
///
/// Every photo source (Google Photos, Amazon Photos, local filesystem, etc.)
/// implements this trait. The main engine interacts exclusively through
/// `dyn PhotoPlugin`, so new sources can be added without touching core code.
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Metadata about a single photo, returned by the plugin.
/// No pixel data — actual bytes are fetched on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoMeta {
    /// Unique, stable identifier within the plugin's namespace.
    pub id: String,
    /// Original filename (for display and caching).
    pub filename: String,
    /// Source-reported width in pixels (may be 0 if unknown).
    pub width: u32,
    /// Source-reported height in pixels (may be 0 if unknown).
    pub height: u32,
    /// When the photo was taken, if available.
    pub taken_at: Option<DateTime<Utc>>,
    /// A URL that can be passed back to `get_photo_bytes`. Plugins may use
    /// this to carry a download URL, or leave it None and resolve via `id`.
    pub download_url: Option<String>,
    /// Free-form key/value bag for plugin-specific metadata.
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

impl PhotoMeta {
    /// Stable cache key: `{plugin_name}/{id}`.
    pub fn cache_key(&self, plugin_name: &str) -> String {
        format!("{}/{}", plugin_name, self.id)
    }
}

/// Plugin-specific configuration, read from `[plugins.<name>]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Arbitrary string key/value pairs.  Plugins document which keys they
    /// expect in their README.
    #[serde(flatten)]
    pub values: HashMap<String, serde_json::Value>,
}

impl PluginConfig {
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.as_str()
    }

    pub fn require_str(&self, key: &str) -> Result<&str> {
        self.get_str(key)
            .ok_or_else(|| anyhow::anyhow!("Plugin config missing required key: {key}"))
    }
}

/// Authentication state reported back to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// Fully authenticated and ready.
    Authenticated,
    /// Need user interaction — show the message on screen and poll.
    PendingUserAction { message: String, poll_interval_secs: u64 },
    /// Not authenticated and cannot proceed without `authenticate()`.
    NotAuthenticated,
}

/// The single trait every photo source plugin must implement.
///
/// # Thread safety
/// Implementations must be `Send + Sync` because the engine may call them
/// from different Tokio tasks (e.g. a background prefetch task).
///
/// # Error handling
/// Return `Err` for transient errors (network timeout, rate limit).  The
/// engine will log and retry. Return `Ok` with an empty Vec for "no results".
#[async_trait]
pub trait PhotoPlugin: Send + Sync {
    /// Short, lowercase, stable identifier used for config keys and cache paths.
    /// Example: `"google-photos"`, `"local"`.
    fn name(&self) -> &str;

    /// Human-readable display name.
    fn display_name(&self) -> &str { self.name() }

    /// Semantic version string.
    fn version(&self) -> &str { "0.1.0" }

    /// Initialise the plugin with its section from config.toml.
    /// Called once at startup before any other method.
    async fn init(&mut self, config: &PluginConfig) -> Result<()>;

    /// Check current authentication status without triggering a network round-trip.
    async fn auth_status(&self) -> AuthStatus;

    /// Begin or continue the authentication flow.
    ///
    /// For headless OAuth 2.0 device flow this writes the verification URL
    /// and user code to screen, then returns `PendingUserAction`. The engine
    /// polls `auth_status()` on the returned interval until `Authenticated`.
    async fn authenticate(&mut self) -> Result<AuthStatus>;

    /// Return up to `limit` photo metadata items starting at `offset`.
    ///
    /// Implementations should honour `offset` so the engine can page through
    /// large libraries without holding everything in memory.
    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>>;

    /// Fetch raw image bytes for a photo at a given display resolution.
    ///
    /// `display_width` / `display_height` are the screen dimensions. Plugins
    /// should request the smallest version from their CDN that is ≥ those
    /// dimensions (saves bandwidth on Pi Zero's slow connection).
    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        display_width: u32,
        display_height: u32,
    ) -> Result<Vec<u8>>;

    /// Called by the engine once per day to refresh tokens or do housekeeping.
    async fn refresh_auth(&mut self) -> Result<()> { Ok(()) }

    /// Called when the engine is shutting down gracefully.
    async fn shutdown(&mut self) -> Result<()> { Ok(()) }
}

/// Type-erased, heap-allocated plugin instance.
pub type BoxedPlugin = Box<dyn PhotoPlugin>;
