use anyhow::Result;
use async_trait::async_trait;
/// Local filesystem plugin for PicoGallery.
///
/// Reads JPEG images from one or more local directories.
/// Supports recursive scanning.
///
/// Config keys:
///   paths     = ["/mnt/photos", "/home/pi/Pictures"]  (required)
///   recursive = true                                   (default: true)
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncReadExt;

use picogallery_core::{
    exif_thumb_from_head, AuthStatus, FetchIntent, PhotoMeta, PhotoPlugin, PluginConfig,
    EXIF_HEAD_SCAN_BYTES,
};

/// Reject images larger than this when reading from disk (guards against OOM).
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024; // 50 MB

pub struct LocalPlugin {
    cfg: PluginConfig,
    paths: Vec<PathBuf>,
    /// Sorted scan result, built once in `init` — avoids re-walking the whole
    /// tree on every `list_photos` page.
    photos: Vec<PathBuf>,
}

impl LocalPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            paths: Vec::new(),
            photos: Vec::new(),
        }
    }

    fn recursive(&self) -> bool {
        self.cfg
            .values
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    async fn scan_dir(
        &self,
        dir: &Path,
        root: &Path,
        visited: &mut HashSet<PathBuf>,
        out: &mut Vec<PathBuf>,
    ) {
        let dir = fs::canonicalize(dir)
            .await
            .unwrap_or_else(|_| dir.to_path_buf());
        let root = fs::canonicalize(root)
            .await
            .unwrap_or_else(|_| root.to_path_buf());
        self.scan_dir_inner(&dir, &root, visited, out).await;
    }

    async fn scan_dir_inner(
        &self,
        dir: &Path,
        root: &Path,
        visited: &mut HashSet<PathBuf>,
        out: &mut Vec<PathBuf>,
    ) {
        const MAX_DIRS: usize = 10_000;
        const MAX_FILES: usize = 100_000;
        if visited.len() >= MAX_DIRS || out.len() >= MAX_FILES {
            return;
        }
        // Symlink cycles inside the root (e.g. photos/loop -> photos/) would
        // recurse forever — skip any canonical dir we've already walked.
        if !visited.insert(dir.to_path_buf()) {
            warn!(
                "Local plugin: symlink cycle detected at {} — skipping",
                dir.display()
            );
            return;
        }

        let mut rd = match fs::read_dir(dir).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Cannot read dir {}: {}", dir.display(), e);
                return;
            }
        };

        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            // Resolve symlinks before further checks to prevent traversal.
            // (Async: a blocking canonicalize would stall the whole runtime.)
            let canonical = match fs::canonicalize(&path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !canonical.starts_with(root) {
                warn!(
                    "Local plugin: rejecting path outside root {}: {}",
                    root.display(),
                    canonical.display()
                );
                continue;
            }
            // PhotoMeta.id is String; PathBuf::from cannot round-trip lossy ids.
            if canonical.to_str().is_none() {
                warn!(
                    "Local plugin: skipping non-UTF-8 path {}",
                    canonical.display()
                );
                continue;
            }
            let is_dir = fs::metadata(&canonical)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir && self.recursive() {
                Box::pin(self.scan_dir_inner(&canonical, root, visited, out)).await;
            } else if is_image(&canonical) && out.len() < MAX_FILES {
                out.push(canonical);
            }
        }
    }
}

/// Expand a leading `~` or `~/` to $HOME. Other `~username` forms pass through
/// unchanged so we avoid spawning a `getpwnam`.
fn expand_home(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

/// Extension-based pre-filter (fast). Magic-byte check happens at read time.
/// JPEG only — the slideshow decoder is built with JPEG support alone.
fn is_image(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg")
    )
}

/// Verify the first bytes of a file match the JPEG signature.
fn has_image_magic(bytes: &[u8]) -> bool {
    matches!(bytes, [0xFF, 0xD8, 0xFF, ..])
}

/// Gallery-thumb shortcut: read the JPEG head and return an embedded EXIF
/// thumbnail when one is large enough. `None` means "read the whole file".
async fn try_gallery_exif_thumb(
    path: &Path,
    file_size: u64,
    intent: FetchIntent,
) -> Result<Option<Vec<u8>>> {
    if !intent.is_thumb() || file_size < 3 {
        return Ok(None);
    }
    let n = (EXIF_HEAD_SCAN_BYTES as u64).min(file_size) as usize;
    let mut file = match fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let mut head = vec![0u8; n];
    if file.read_exact(&mut head).await.is_err() {
        return Ok(None);
    }
    if !has_image_magic(&head) {
        return Ok(None);
    }
    Ok(exif_thumb_from_head(&head, intent.target_edge()))
}

async fn file_mtime_utc(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0))
}

#[async_trait]
impl PhotoPlugin for LocalPlugin {
    fn name(&self) -> &str {
        "local"
    }
    fn uses_engine_image_cache(&self) -> bool {
        false
    }
    fn display_name(&self) -> &str {
        "Local filesystem"
    }

    async fn init(&mut self, config: &PluginConfig) -> Result<()> {
        self.cfg = config.clone();
        if let Some(arr) = self.cfg.values.get("paths").and_then(|v| v.as_array()) {
            let mut paths = Vec::new();
            for s in arr.iter().filter_map(|v| v.as_str()) {
                // Expand `~` to $HOME before canonicalize so user-agnostic
                // configs (e.g. "~/Pictures") work on any host.
                let expanded = expand_home(s);
                match fs::canonicalize(&expanded).await {
                    Ok(c) => paths.push(c),
                    Err(e) => warn!("Local plugin: skipping path '{}': {}", s, e),
                }
            }
            self.paths = paths;
        }
        // Log only path count, not full paths (could contain username etc.)
        info!("Local plugin: {} configured path(s).", self.paths.len());

        // Scan once at startup. Sorted so paging is deterministic
        // (read_dir order is filesystem-dependent).
        let mut all = Vec::new();
        // One visited set across all roots so overlapping configured paths
        // are listed once instead of duplicated.
        let mut visited = HashSet::new();
        for dir in &self.paths {
            self.scan_dir(dir, dir, &mut visited, &mut all).await;
        }
        all.sort();
        info!("Local plugin: {} photos scanned.", all.len());
        self.photos = all;
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::Authenticated
    }
    async fn authenticate(&mut self) -> Result<AuthStatus> {
        Ok(AuthStatus::Authenticated)
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let candidates: Vec<(PathBuf, String, String)> = self
            .photos
            .iter()
            // Guard BEFORE paging: paths in self.photos are already
            // canonicalize-validated at scan time, but if one ever slipped
            // through, rejecting it after skip/take would silently shrink the
            // page and make the engine stop paging early.
            .filter(|path| {
                let allowed = self.paths.iter().any(|root| path.starts_with(root));
                if !allowed {
                    warn!(
                        "Local plugin: rejecting out-of-root path {}",
                        path.display()
                    );
                }
                allowed
            })
            .skip(offset)
            .take(limit)
            .filter_map(|path| {
                let id = path.to_str()?.to_string();
                let filename = path.file_name()?.to_str()?.to_string();
                Some((path.clone(), id, filename))
            })
            .collect();
        let mut page = Vec::with_capacity(candidates.len());
        for (path, id, filename) in candidates {
            let taken_at = file_mtime_utc(&path).await;
            page.push(PhotoMeta {
                id,
                filename,
                width: 0,
                height: 0,
                taken_at,
                download_url: None, // bytes are read directly in get_photo_bytes
                album: None,
                title: None,
                location: None,
                is_favorite: false,
                extra: Default::default(),
            });
        }

        Ok(page)
    }

    async fn get_photo_bytes(&self, meta: &PhotoMeta, intent: FetchIntent) -> Result<Vec<u8>> {
        let path = PathBuf::from(&meta.id);

        // Re-canonicalize and re-validate at read time (symlinks could have been swapped).
        let canonical = fs::canonicalize(&path)
            .await
            .map_err(|e| anyhow::anyhow!("resolving path {}: {}", path.display(), e))?;

        let allowed = self.paths.iter().any(|root| canonical.starts_with(root));
        if !allowed {
            return Err(anyhow::anyhow!(
                "security: {} is outside configured paths",
                canonical.display()
            ));
        }

        // Size check before loading into memory.
        let meta_data = fs::metadata(&canonical)
            .await
            .map_err(|e| anyhow::anyhow!("stat {}: {}", canonical.display(), e))?;
        if meta_data.len() > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "file too large ({} MB): {}",
                meta_data.len() / 1_048_576,
                canonical.display()
            ));
        }

        if let Some(thumb) = try_gallery_exif_thumb(&canonical, meta_data.len(), intent).await? {
            return Ok(thumb);
        }

        let bytes = fs::read(&canonical)
            .await
            .map_err(|e| anyhow::anyhow!("reading {}: {}", canonical.display(), e))?;

        // Magic-byte validation — reject files that don't look like images.
        if !has_image_magic(&bytes) {
            return Err(anyhow::anyhow!(
                "file does not match any known image format: {}",
                canonical.display()
            ));
        }

        debug!("Read {} bytes from {}", bytes.len(), canonical.display());
        Ok(bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_expand_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("/foo/bar"), PathBuf::from("/foo/bar"));
        assert_eq!(expand_home("~foo/bar"), PathBuf::from("~foo/bar")); // not expanded by design
    }

    #[test]
    fn test_is_image() {
        assert!(is_image(Path::new("a.jpg")));
        assert!(is_image(Path::new("b.jpeg")));
        assert!(is_image(Path::new("C.JPG")));
        assert!(!is_image(Path::new("d.png")));
        assert!(!is_image(Path::new("e.txt")));
        assert!(!is_image(Path::new("no_extension")));
    }

    #[test]
    fn test_has_image_magic() {
        assert!(has_image_magic(&[0xFF, 0xD8, 0xFF, 0x00]));
        assert!(!has_image_magic(&[0xFF, 0xD8, 0x00, 0x00]));
        assert!(!has_image_magic(&[]));
    }

    #[tokio::test]
    async fn test_scan_dir() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let d1 = root.join("dir1");
        fs::create_dir(&d1).await.unwrap();
        let f1 = root.join("1.jpg");
        let f2 = d1.join("2.jpg");
        let f3 = root.join("3.png");
        fs::write(&f1, b"jpg").await.unwrap();
        fs::write(&f2, b"jpg").await.unwrap();
        fs::write(&f3, b"png").await.unwrap();

        let plugin = LocalPlugin::new(PluginConfig::default());
        let mut visited = HashSet::new();
        let mut out = Vec::new();

        // recursive scan
        plugin.scan_dir(root, root, &mut visited, &mut out).await;
        assert_eq!(out.len(), 2);

        let out_paths: Vec<_> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(out_paths.contains(&"1.jpg".to_string()));
        assert!(out_paths.contains(&"2.jpg".to_string()));
    }

    #[tokio::test]
    async fn test_scan_dir_symlink_cycle() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let d1 = root.join("dir1");
        fs::create_dir(&d1).await.unwrap();

        // create symlink inside dir1 pointing to root
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, d1.join("link")).unwrap();

        let plugin = LocalPlugin::new(PluginConfig::default());
        let mut visited = HashSet::new();
        let mut out = Vec::new();
        plugin.scan_dir(root, root, &mut visited, &mut out).await;
        // Should not panic or hang
    }

    #[tokio::test]
    async fn test_init_and_list_photos() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let f1 = root.join("1.jpg");
        fs::write(&f1, b"").await.unwrap();

        let mut cfg = PluginConfig::default();
        cfg.values.insert(
            "paths".to_string(),
            serde_json::json!([root.to_string_lossy().to_string()]),
        );

        let mut plugin = LocalPlugin::new(cfg);
        plugin.init(&plugin.cfg.clone()).await.unwrap();

        assert_eq!(plugin.photos.len(), 1);

        let meta = plugin.list_photos(10, 0).await.unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].filename, "1.jpg");
    }

    #[tokio::test]
    async fn test_get_photo_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let f1 = root.join("1.jpg");
        fs::write(&f1, [0xFF, 0xD8, 0xFF, 0x12, 0x34])
            .await
            .unwrap();

        let mut cfg = PluginConfig::default();
        cfg.values.insert(
            "paths".to_string(),
            serde_json::json!([root.to_string_lossy().to_string()]),
        );

        let mut plugin = LocalPlugin::new(cfg);
        plugin.init(&plugin.cfg.clone()).await.unwrap();

        let meta = plugin.list_photos(10, 0).await.unwrap();
        let bytes = plugin
            .get_photo_bytes(
                &meta[0],
                FetchIntent::Fullscreen {
                    display_width: 1920,
                    display_height: 1080,
                },
            )
            .await
            .unwrap();
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, 0x12, 0x34]);
    }

    #[tokio::test]
    async fn test_get_photo_bytes_rejects_non_magic() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let f1 = root.join("1.jpg");
        fs::write(&f1, [0x00, 0x00, 0x00]).await.unwrap(); // not jpeg

        let mut cfg = PluginConfig::default();
        cfg.values.insert(
            "paths".to_string(),
            serde_json::json!([root.to_string_lossy().to_string()]),
        );

        let mut plugin = LocalPlugin::new(cfg);
        plugin.init(&plugin.cfg.clone()).await.unwrap();

        let meta = plugin.list_photos(10, 0).await.unwrap();
        let err = plugin
            .get_photo_bytes(
                &meta[0],
                FetchIntent::Fullscreen {
                    display_width: 1920,
                    display_height: 1080,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("known image format"));
    }
}
