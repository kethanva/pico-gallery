/// Directory plugin for PicoGallery.
///
/// Serves photos from a single root directory. Sub-directories are treated as
/// "albums" (analogous to Google Photos albums). Photos can be displayed in
/// shuffle, alphabetical, or date-modified order.
///
/// Config keys (in `[[plugins]]`):
///
/// ```toml
/// [[plugins]]
/// name    = "directory"
/// enabled = true
/// path    = "/home/pi/Photos"           # required — root directory to scan
/// order   = "shuffle"                   # "shuffle" | "alphabetical" | "date_modified"
/// recursive = true                      # scan sub-directories (default: true)
/// allowed_albums = ["Vacation", "2024"] # optional allowlist of sub-dirs; empty = all
/// rescan_interval_secs = 3600           # re-scan every N seconds; 0 = startup only
/// ```
use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::RwLock;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024; // 50 MB guard (matches other plugins)

// ── Ordering ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Order {
    Shuffle,
    Alphabetical,
    DateModified,
}

impl Order {
    fn from_cfg(cfg: &PluginConfig) -> Self {
        match cfg.get_str("order").unwrap_or("shuffle") {
            "alphabetical"  => Self::Alphabetical,
            "date_modified" => Self::DateModified,
            _               => Self::Shuffle,
        }
    }
}

// ── Internal photo record ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ScannedPhoto {
    /// Canonicalized absolute path on disk.
    path: PathBuf,
    /// Name of the immediate sub-directory under root, if any (= "album").
    album: Option<String>,
    /// Last-modified time as UNIX seconds (used for `DateModified` ordering).
    modified_secs: u64,
}

impl ScannedPhoto {
    fn into_meta(self, idx: usize) -> PhotoMeta {
        let filename = self.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let mut extra = HashMap::new();
        if let Some(album) = self.album {
            extra.insert("album".to_string(), album);
        }

        PhotoMeta {
            id:           idx.to_string(),
            filename,
            width:        0,
            height:       0,
            taken_at:     None,
            // Store local path in download_url so get_photo_bytes can read it.
            download_url: Some(self.path.to_string_lossy().to_string()),
            extra,
        }
    }
}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct DirectoryPlugin {
    cfg:    PluginConfig,
    /// Canonicalized root directory; set by `init`.
    root:   Option<PathBuf>,
    /// Sorted/shuffled list of photos, refreshed by `init` (and future rescans).
    photos: RwLock<Vec<ScannedPhoto>>,
}

impl DirectoryPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            root:   None,
            photos: RwLock::new(Vec::new()),
        }
    }

    // ── Config helpers ────────────────────────────────────────────────────────

    fn recursive(&self) -> bool {
        self.cfg.values.get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    fn rescan_interval_secs(&self) -> u64 {
        self.cfg.values.get("rescan_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }

    /// Returns the album allowlist, or an empty vec meaning "all albums".
    fn allowed_albums(&self) -> Vec<String> {
        self.cfg.values
            .get("allowed_albums")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ── Scanning ──────────────────────────────────────────────────────────────

    /// Walk `dir` recursively, collecting image files into `out`.
    ///
    /// `album` is the name of the first-level sub-directory under root — that
    /// becomes the "album" label for every photo beneath it.
    async fn scan_dir(
        &self,
        root:    &Path,
        dir:     &Path,
        album:   Option<&str>,
        allowed: &[String],
        out:     &mut Vec<ScannedPhoto>,
    ) {
        let mut rd = match fs::read_dir(dir).await {
            Ok(r)  => r,
            Err(e) => {
                warn!("Directory plugin: cannot read {}: {}", dir.display(), e);
                return;
            }
        };

        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();

            // Resolve symlinks; reject anything that escapes the root.
            let canonical = match path.canonicalize() {
                Ok(c)  => c,
                Err(_) => continue,
            };
            if !canonical.starts_with(root) {
                warn!(
                    "Directory plugin: symlink escape rejected — {}",
                    canonical.display()
                );
                continue;
            }

            if canonical.is_dir() {
                if !self.recursive() {
                    continue;
                }

                let dir_name = canonical
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                // Apply allowlist only at the top level (album = first sub-dir).
                if album.is_none() && !allowed.is_empty() && !allowed.contains(&dir_name) {
                    debug!("Directory plugin: skipping album '{}' (not in allowed_albums)", dir_name);
                    continue;
                }

                // First-level sub-dir becomes the album; nested dirs inherit it.
                let new_album = if album.is_none() {
                    Some(dir_name.as_str())
                } else {
                    album
                };

                Box::pin(self.scan_dir(root, &canonical, new_album, allowed, out)).await;
            } else if is_image(&canonical) {
                let modified_secs = fs::metadata(&canonical)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                out.push(ScannedPhoto {
                    path: canonical,
                    album: album.map(str::to_string),
                    modified_secs,
                });
            }
        }
    }

    async fn build_photo_list(&self) -> Vec<ScannedPhoto> {
        let root = match &self.root {
            Some(r) => r.clone(),
            None    => return vec![],
        };

        let allowed = self.allowed_albums();
        let mut photos = Vec::new();
        self.scan_dir(&root, &root, None, &allowed, &mut photos).await;

        let order = Order::from_cfg(&self.cfg);
        match order {
            Order::Alphabetical => {
                photos.sort_by(|a, b| a.path.cmp(&b.path));
            }
            Order::DateModified => {
                // Newest first.
                photos.sort_by(|a, b| b.modified_secs.cmp(&a.modified_secs));
            }
            Order::Shuffle => {
                let seed = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(42);
                shuffle(&mut photos, seed);
            }
        }

        info!(
            "Directory plugin: {} photos scanned (order: {:?})",
            photos.len(),
            order
        );
        photos
    }
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for DirectoryPlugin {
    fn name(&self)         -> &str { "directory"       }
    fn display_name(&self) -> &str { "Local Directory" }
    fn version(&self)      -> &str { "0.1.0"           }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        // Resolve and validate the root path.
        let path_str = self.cfg.require_str("path")
            .context("directory plugin requires a 'path' config key")?;

        let canonical = PathBuf::from(path_str)
            .canonicalize()
            .with_context(|| format!("directory plugin: cannot resolve path '{path_str}'"))?;

        if !canonical.is_dir() {
            return Err(anyhow::anyhow!(
                "directory plugin: '{}' is not a directory",
                canonical.display()
            ));
        }

        info!("Directory plugin: root = {}", canonical.display());
        self.root = Some(canonical);

        // Initial scan.
        let photos = self.build_photo_list().await;
        *self.photos.write().await = photos;

        let interval = self.rescan_interval_secs();
        if interval > 0 {
            info!(
                "Directory plugin: rescan_interval_secs = {interval} \
                 (restart the app to pick up new photos in this version)"
            );
        }

        Ok(())
    }

    // No auth needed for local filesystem.
    async fn auth_status(&self)         -> AuthStatus         { AuthStatus::Authenticated }
    async fn authenticate(&mut self)    -> Result<AuthStatus> { Ok(AuthStatus::Authenticated) }
    async fn refresh_auth(&mut self)    -> Result<()>         { Ok(()) }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let photos = self.photos.read().await;
        let page = photos
            .iter()
            .cloned()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, p)| p.into_meta(offset + i))
            .collect();
        Ok(page)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _display_width:  u32,
        _display_height: u32,
    ) -> Result<Vec<u8>> {
        let path_str = meta.download_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("directory plugin: no path stored for '{}'", meta.filename))?;

        // Re-canonicalize at read time (guards against symlink swap attacks).
        let canonical = PathBuf::from(path_str)
            .canonicalize()
            .with_context(|| format!("resolving '{path_str}'"))?;

        // Must still be inside the configured root.
        let inside_root = self.root
            .as_ref()
            .map(|r| canonical.starts_with(r))
            .unwrap_or(false);
        if !inside_root {
            return Err(anyhow::anyhow!(
                "security: '{}' is outside the configured directory",
                canonical.display()
            ));
        }

        // Size guard before reading into memory.
        let file_size = fs::metadata(&canonical)
            .await
            .with_context(|| format!("stat '{path_str}'"))?
            .len();
        if file_size > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "file too large ({} MB): '{}'",
                file_size / 1_048_576,
                canonical.display()
            ));
        }

        let bytes = fs::read(&canonical)
            .await
            .with_context(|| format!("reading '{path_str}'"))?;

        // Magic-byte validation — extension alone is insufficient.
        if !has_image_magic(&bytes) {
            return Err(anyhow::anyhow!(
                "not a recognised image format: '{}'",
                canonical.display()
            ));
        }

        debug!("Directory plugin: read {} bytes from {}", bytes.len(), canonical.display());
        Ok(bytes)
    }
}

// ── Image helpers ─────────────────────────────────────────────────────────────

/// Extension pre-filter (cheap). Magic-byte check happens at read time.
fn is_image(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "webp")
    )
}

/// First-bytes check against known image format signatures.
fn has_image_magic(bytes: &[u8]) -> bool {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..]                              => true, // JPEG
        [0x89, b'P', b'N', b'G', ..]                        => true, // PNG
        [b'G', b'I', b'F', b'8', ..]                        => true, // GIF
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => true, // WebP
        _                                                    => false,
    }
}

// ── Fisher-Yates shuffle (no rand dep) ────────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_from_cfg_defaults_to_shuffle() {
        let cfg = PluginConfig::default();
        assert_eq!(Order::from_cfg(&cfg), Order::Shuffle);
    }

    #[test]
    fn order_from_cfg_alphabetical() {
        let mut cfg = PluginConfig::default();
        cfg.values.insert("order".to_string(), serde_json::json!("alphabetical"));
        assert_eq!(Order::from_cfg(&cfg), Order::Alphabetical);
    }

    #[test]
    fn order_from_cfg_date_modified() {
        let mut cfg = PluginConfig::default();
        cfg.values.insert("order".to_string(), serde_json::json!("date_modified"));
        assert_eq!(Order::from_cfg(&cfg), Order::DateModified);
    }

    #[test]
    fn order_from_cfg_unknown_falls_back_to_shuffle() {
        let mut cfg = PluginConfig::default();
        cfg.values.insert("order".to_string(), serde_json::json!("random_walk"));
        assert_eq!(Order::from_cfg(&cfg), Order::Shuffle);
    }

    #[test]
    fn is_image_recognises_common_extensions() {
        assert!(is_image(Path::new("photo.jpg")));
        assert!(is_image(Path::new("photo.JPEG")));
        assert!(is_image(Path::new("photo.png")));
        assert!(is_image(Path::new("photo.gif")));
        assert!(is_image(Path::new("photo.webp")));
        assert!(!is_image(Path::new("document.pdf")));
        assert!(!is_image(Path::new("video.mp4")));
        assert!(!is_image(Path::new("noext")));
    }

    #[test]
    fn has_image_magic_jpeg() {
        let jpeg_header = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert!(has_image_magic(&jpeg_header));
    }

    #[test]
    fn has_image_magic_png() {
        let png_header = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert!(has_image_magic(&png_header));
    }

    #[test]
    fn has_image_magic_rejects_text() {
        let text = b"Hello, world!";
        assert!(!has_image_magic(text));
    }

    #[test]
    fn shuffle_deterministic_with_same_seed() {
        let mut a = vec![1, 2, 3, 4, 5];
        let mut b = vec![1, 2, 3, 4, 5];
        shuffle(&mut a, 12345);
        shuffle(&mut b, 12345);
        assert_eq!(a, b);
    }

    #[test]
    fn shuffle_different_seeds_produce_different_orders() {
        let mut a = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let mut b = a.clone();
        shuffle(&mut a, 1);
        shuffle(&mut b, 9999999);
        assert_ne!(a, b);
    }

    #[test]
    fn scanned_photo_into_meta_carries_album() {
        let photo = ScannedPhoto {
            path:          PathBuf::from("/photos/Vacation/img.jpg"),
            album:         Some("Vacation".to_string()),
            modified_secs: 0,
        };
        let meta = photo.into_meta(3);
        assert_eq!(meta.id, "3");
        assert_eq!(meta.filename, "img.jpg");
        assert_eq!(meta.extra.get("album").map(String::as_str), Some("Vacation"));
        assert_eq!(meta.download_url.as_deref(), Some("/photos/Vacation/img.jpg"));
    }

    #[test]
    fn scanned_photo_into_meta_no_album() {
        let photo = ScannedPhoto {
            path:          PathBuf::from("/photos/img.jpg"),
            album:         None,
            modified_secs: 0,
        };
        let meta = photo.into_meta(0);
        assert!(!meta.extra.contains_key("album"));
    }

    #[tokio::test]
    async fn init_fails_with_missing_path_key() {
        let mut plugin = DirectoryPlugin::new(PluginConfig::default());
        let cfg = PluginConfig::default();
        let result = plugin.init(&cfg).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path"));
    }

    #[tokio::test]
    async fn init_fails_for_nonexistent_directory() {
        let mut cfg = PluginConfig::default();
        cfg.values.insert("path".to_string(), serde_json::json!("/this/does/not/exist/ever"));
        let mut plugin = DirectoryPlugin::new(cfg.clone());
        let result = plugin.init(&cfg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn auth_status_is_always_authenticated() {
        let plugin = DirectoryPlugin::new(PluginConfig::default());
        assert_eq!(plugin.auth_status().await, AuthStatus::Authenticated);
    }

    #[tokio::test]
    async fn list_photos_empty_before_init() {
        let plugin = DirectoryPlugin::new(PluginConfig::default());
        let photos = plugin.list_photos(10, 0).await.unwrap();
        assert!(photos.is_empty());
    }
}
