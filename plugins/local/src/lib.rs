/// Local filesystem plugin for PicoGallery.
///
/// Reads JPEG/PNG/WebP/GIF images from one or more local directories.
/// Supports recursive scanning.
///
/// Config keys:
///   paths     = ["/mnt/photos", "/home/pi/Pictures"]  (required)
///   recursive = true                                   (default: true)
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info, warn};
use std::path::{Path, PathBuf};
use tokio::fs;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

/// Reject images larger than this when reading from disk (guards against OOM).
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024; // 50 MB

pub struct LocalPlugin {
    cfg:   PluginConfig,
    paths: Vec<PathBuf>,
}

impl LocalPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self { cfg, paths: Vec::new() }
    }

    fn recursive(&self) -> bool {
        self.cfg.values.get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    async fn scan_dir(&self, dir: &Path, out: &mut Vec<PathBuf>) {
        let mut rd = match fs::read_dir(dir).await {
            Ok(r) => r,
            Err(e) => { warn!("Cannot read dir {}: {}", dir.display(), e); return; }
        };

        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            // Resolve symlinks before further checks to prevent traversal.
            let canonical = match path.canonicalize() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if canonical.is_dir() && self.recursive() {
                Box::pin(self.scan_dir(&canonical, out)).await;
            } else if is_image(&canonical) {
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
fn is_image(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "webp")
    )
}

/// Verify the first bytes of a file match a known image format signature.
fn has_image_magic(bytes: &[u8]) -> bool {
    match bytes {
        // JPEG
        [0xFF, 0xD8, 0xFF, ..] => true,
        // PNG
        [0x89, b'P', b'N', b'G', ..] => true,
        // GIF87a / GIF89a
        [b'G', b'I', b'F', b'8', ..] => true,
        // WebP (RIFF....WEBP)
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => true,
        _ => false,
    }
}

#[async_trait]
impl PhotoPlugin for LocalPlugin {
    fn name(&self)         -> &str { "local"           }
    fn display_name(&self) -> &str { "Local filesystem" }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        if let Some(arr) = self.cfg.values.get("paths").and_then(|v| v.as_array()) {
            self.paths = arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| {
                    // Expand `~` to $HOME before canonicalize so user-agnostic
                    // configs (e.g. "~/Pictures") work on any host.
                    let expanded = expand_home(s);
                    match expanded.canonicalize() {
                        Ok(c) => Some(c),
                        Err(e) => {
                            warn!("Local plugin: skipping path '{}': {}", s, e);
                            None
                        }
                    }
                })
                .collect();
        }
        // Log only path count, not full paths (could contain username etc.)
        info!("Local plugin: {} configured path(s).", self.paths.len());
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus { AuthStatus::Authenticated }
    async fn authenticate(&mut self) -> Result<AuthStatus> { Ok(AuthStatus::Authenticated) }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let mut all = Vec::new();
        for dir in &self.paths {
            self.scan_dir(dir, &mut all).await;
        }

        let page: Vec<PhotoMeta> = all.into_iter()
            .skip(offset)
            .take(limit)
            .filter_map(|path| {
                // Guard: verify scanned path still begins with one of our configured roots.
                let allowed = self.paths.iter().any(|root| path.starts_with(root));
                if !allowed {
                    warn!("Local plugin: rejecting out-of-root path {}", path.display());
                    return None;
                }
                let id = path.to_string_lossy().to_string();
                let filename = path.file_name()?.to_string_lossy().to_string();
                Some(PhotoMeta {
                    id,
                    filename,
                    width:        0,
                    height:       0,
                    taken_at:     None,
                    download_url: None, // bytes are read directly in get_photo_bytes
                    extra:        Default::default(),
                })
            })
            .collect();

        Ok(page)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _dw: u32,
        _dh: u32,
    ) -> Result<Vec<u8>> {
        let path = PathBuf::from(&meta.id);

        // Re-canonicalize and re-validate at read time (symlinks could have been swapped).
        let canonical = path.canonicalize()
            .map_err(|e| anyhow::anyhow!("resolving path {}: {}", path.display(), e))?;

        let allowed = self.paths.iter().any(|root| canonical.starts_with(root));
        if !allowed {
            return Err(anyhow::anyhow!(
                "security: {} is outside configured paths", canonical.display()
            ));
        }

        // Size check before loading into memory.
        let meta_data = fs::metadata(&canonical).await
            .map_err(|e| anyhow::anyhow!("stat {}: {}", canonical.display(), e))?;
        if meta_data.len() > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "file too large ({} MB): {}", meta_data.len() / 1_048_576, canonical.display()
            ));
        }

        let bytes = fs::read(&canonical).await
            .map_err(|e| anyhow::anyhow!("reading {}: {}", canonical.display(), e))?;

        // Magic-byte validation — reject files that don't look like images.
        if !has_image_magic(&bytes) {
            return Err(anyhow::anyhow!(
                "file does not match any known image format: {}", canonical.display()
            ));
        }

        debug!("Read {} bytes from {}", bytes.len(), canonical.display());
        Ok(bytes)
    }
}
