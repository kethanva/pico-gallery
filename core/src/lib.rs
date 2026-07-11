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
    /// Album / folder label for OSD and remote status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Human title when it differs from the filename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Place string, e.g. "City, Country".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Whether the source marks this photo as a favourite.
    #[serde(default)]
    pub is_favorite: bool,
    /// Plugin-private transport bag (hashes, tokens, paths). Not for display.
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

/// Live album / favourites filter applied by the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetingState {
    /// Album id/slug/folder name. Empty = all albums.
    pub album: String,
    /// When true, only favourited photos (if the plugin supports it).
    pub favorites_only: bool,
}

/// How a plugin maps [`TargetingState`] into its `PluginConfig` keys.
///
/// Declared by the plugin so the engine never hard-codes plugin names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TargetingAdapter {
    /// Config key for the album filter. `None` = no album targeting.
    pub album_key: Option<&'static str>,
    /// When true, write the album as a single-element JSON array.
    pub album_as_array: bool,
    /// Config key for a favourites-only bool. `None` = no favourites filter.
    pub favorites_key: Option<&'static str>,
}

impl TargetingAdapter {
    pub const NONE: Self = Self {
        album_key: None,
        album_as_array: false,
        favorites_key: None,
    };

    pub fn supports_albums(self) -> bool {
        self.album_key.is_some()
    }

    pub fn supports_favorites_filter(self) -> bool {
        self.favorites_key.is_some()
    }

    /// Write `targeting` into `config` using this adapter's keys.
    pub fn apply(self, targeting: &TargetingState, config: &mut PluginConfig) {
        if let Some(key) = self.album_key {
            if targeting.album.is_empty() {
                config.values.remove(key);
            } else if self.album_as_array {
                config
                    .values
                    .insert(key.into(), serde_json::json!([targeting.album]));
            } else {
                config.values.insert(
                    key.into(),
                    serde_json::Value::String(targeting.album.clone()),
                );
            }
        }
        if let Some(key) = self.favorites_key {
            if targeting.favorites_only {
                config
                    .values
                    .insert(key.into(), serde_json::Value::Bool(true));
            } else {
                config.values.remove(key);
            }
        }
    }

    /// Read album / favourites back from `config` (empty / false when unset).
    pub fn read(self, config: &PluginConfig) -> TargetingState {
        let album = self
            .album_key
            .map(|key| {
                if self.album_as_array {
                    config
                        .values
                        .get(key)
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    config.get_str(key).unwrap_or("").to_string()
                }
            })
            .unwrap_or_default();
        let favorites_only = self
            .favorites_key
            .and_then(|key| config.values.get(key).and_then(|v| v.as_bool()))
            .unwrap_or(false);
        TargetingState {
            album,
            favorites_only,
        }
    }
}

/// Editable connection setting advertised by a plugin for the settings menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionField {
    /// Key in [`PluginConfig::values`].
    pub key: &'static str,
    /// Human label shown in the menu.
    pub label: &'static str,
    /// When true, values are masked and edit buffers start empty.
    pub secret: bool,
}

/// What the engine may offer for this plugin in menus and remote controls.
#[derive(Debug, Clone, Default)]
pub struct PluginCapabilities {
    /// How album / favourites targeting maps into this plugin's config.
    pub targeting: TargetingAdapter,
    /// Per-photo favourite toggle via [`PhotoPlugin::set_favorite`].
    pub favorite_toggle: bool,
    /// Editable connection fields (URL, credentials, …).
    pub connection_fields: Vec<ConnectionField>,
    /// Label for a reconnect / apply-credentials action. `None` hides it.
    pub reconnect_label: Option<&'static str>,
}

impl PluginCapabilities {
    pub fn supports_targeting(&self) -> bool {
        self.targeting.supports_albums() || self.targeting.supports_favorites_filter()
    }

    pub fn has_connection_ui(&self) -> bool {
        !self.connection_fields.is_empty() || self.reconnect_label.is_some()
    }
}

/// Authentication state reported back to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// Fully authenticated and ready.
    Authenticated,
    /// Need user interaction — show the message on screen and poll.
    PendingUserAction {
        message: String,
        poll_interval_secs: u64,
    },
    /// Not authenticated and cannot proceed without `authenticate()`.
    NotAuthenticated,
}

/// The single trait every photo source plugin must implement.
///
/// # Thread safety
/// Implementations must be `Send + Sync` because the engine may call them
/// from async call sites on the single-threaded runtime (e.g. the HTTP remote listener).
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
    fn display_name(&self) -> &str {
        self.name()
    }

    /// Semantic version string.
    fn version(&self) -> &str {
        "0.1.0"
    }

    /// Capability descriptor for menus, targeting, and remote controls.
    fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities::default()
    }

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
    ///
    /// Backends that only expose token-based pagination may emulate offset
    /// paging internally. Engines must treat a short or empty page as a
    /// possible end-of-library, not as an error.
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

    /// Return browseable albums as `(id, title)` pairs. Empty when unsupported.
    async fn list_albums(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    /// Mark a photo as a favourite in the source (or clear the mark).
    ///
    /// Takes `&self` (not `&mut self`) so the engine can call it from the
    /// display loop while holding only a shared borrow of the plugin list;
    /// implementations use interior mutability for any session state.
    ///
    /// Default: unsupported. Backends without a favourites concept (plain
    /// directories, WebDAV, …) inherit this and return an error, which the
    /// engine logs without disrupting the slideshow.
    async fn set_favorite(&self, _meta: &PhotoMeta, _favorite: bool) -> Result<()> {
        Err(anyhow::anyhow!(
            "this photo source does not support favourites"
        ))
    }

    /// Called by the engine once per day to refresh tokens or do housekeeping.
    async fn refresh_auth(&mut self) -> Result<()> {
        Ok(())
    }

    /// Called when the engine is shutting down gracefully.
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Type-erased, heap-allocated plugin instance.
pub type BoxedPlugin = Box<dyn PhotoPlugin>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_meta_defaults_typed_fields() {
        let meta: PhotoMeta = serde_json::from_value(serde_json::json!({
            "id": "1",
            "filename": "a.jpg",
            "width": 0,
            "height": 0,
            "taken_at": null,
            "download_url": null
        }))
        .expect("deserialize legacy PhotoMeta");
        assert!(meta.album.is_none());
        assert!(meta.title.is_none());
        assert!(meta.location.is_none());
        assert!(!meta.is_favorite);
        assert!(meta.extra.is_empty());
    }

    #[test]
    fn targeting_adapter_photoprism_style() {
        let adapter = TargetingAdapter {
            album_key: Some("album"),
            album_as_array: false,
            favorites_key: Some("favorites"),
        };
        let mut cfg = PluginConfig::default();
        adapter.apply(
            &TargetingState {
                album: "vacation".into(),
                favorites_only: true,
            },
            &mut cfg,
        );
        assert_eq!(cfg.get_str("album"), Some("vacation"));
        assert_eq!(
            cfg.values.get("favorites").and_then(|v| v.as_bool()),
            Some(true)
        );
        adapter.apply(&TargetingState::default(), &mut cfg);
        assert!(cfg.get_str("album").is_none());
        assert!(!cfg.values.contains_key("favorites"));
    }

    #[test]
    fn targeting_adapter_directory_style_array() {
        let adapter = TargetingAdapter {
            album_key: Some("allowed_albums"),
            album_as_array: true,
            favorites_key: None,
        };
        let mut cfg = PluginConfig::default();
        adapter.apply(
            &TargetingState {
                album: "Holiday".into(),
                favorites_only: false,
            },
            &mut cfg,
        );
        let read = adapter.read(&cfg);
        assert_eq!(read.album, "Holiday");
        assert!(!read.favorites_only);
    }

    struct MockPlugin;

    #[async_trait]
    impl PhotoPlugin for MockPlugin {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> PluginCapabilities {
            PluginCapabilities {
                targeting: TargetingAdapter {
                    album_key: Some("album"),
                    album_as_array: false,
                    favorites_key: Some("favorites"),
                },
                favorite_toggle: true,
                connection_fields: vec![ConnectionField {
                    key: "url",
                    label: "URL",
                    secret: false,
                }],
                reconnect_label: Some("Reconnect"),
            }
        }

        async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
            Ok(())
        }

        async fn auth_status(&self) -> AuthStatus {
            AuthStatus::Authenticated
        }

        async fn authenticate(&mut self) -> Result<AuthStatus> {
            Ok(AuthStatus::Authenticated)
        }

        async fn list_photos(&self, _limit: usize, _offset: usize) -> Result<Vec<PhotoMeta>> {
            Ok(vec![PhotoMeta {
                id: "p1".into(),
                filename: "p1.jpg".into(),
                width: 100,
                height: 80,
                taken_at: None,
                download_url: None,
                album: Some("A".into()),
                title: Some("Title".into()),
                location: None,
                is_favorite: true,
                extra: HashMap::new(),
            }])
        }

        async fn get_photo_bytes(&self, _meta: &PhotoMeta, _dw: u32, _dh: u32) -> Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8, 0xFF])
        }
    }

    #[tokio::test]
    async fn mock_plugin_exposes_capabilities_and_typed_meta() {
        let plugin = MockPlugin;
        let caps = plugin.capabilities();
        assert!(caps.supports_targeting());
        assert!(caps.favorite_toggle);
        assert!(caps.has_connection_ui());
        let photos = plugin.list_photos(10, 0).await.unwrap();
        assert_eq!(photos[0].album.as_deref(), Some("A"));
        assert!(photos[0].is_favorite);
    }
}
