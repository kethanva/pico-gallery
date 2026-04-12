/// Local filesystem plugin for PicoGallery.
///
/// Reads JPEG/PNG/WebP/GIF images from one or more local directories.
/// Supports recursive scanning.
///
/// Config keys:
///   paths = ["/mnt/photos", "/home/pi/Pictures"]  (required)
///   recursive = true                               (default: true)
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info, warn};
use std::path::PathBuf;
use tokio::fs;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

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

    async fn scan_dir(&self, dir: &PathBuf, out: &mut Vec<PathBuf>) {
        let mut rd = match fs::read_dir(dir).await {
            Ok(r) => r,
            Err(e) => { warn!("Cannot read dir {}: {}", dir.display(), e); return; }
        };

        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.is_dir() && self.recursive() {
                Box::pin(self.scan_dir(&path, out)).await;
            } else if is_image(&path) {
                out.push(path);
            }
        }
    }
}

fn is_image(p: &PathBuf) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
        Some("jpg") | Some("jpeg") | Some("png") | Some("gif") | Some("webp")
    )
}

#[async_trait]
impl PhotoPlugin for LocalPlugin {
    fn name(&self)         -> &str { "local"           }
    fn display_name(&self) -> &str { "Local filesystem" }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        if let Some(arr) = self.cfg.values.get("paths").and_then(|v| v.as_array()) {
            self.paths = arr.iter()
                .filter_map(|v| v.as_str())
                .map(PathBuf::from)
                .collect();
        }
        info!("Local plugin paths: {:?}", self.paths);
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::Authenticated // no auth needed
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        Ok(AuthStatus::Authenticated)
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let mut all = Vec::new();
        for dir in &self.paths {
            self.scan_dir(dir, &mut all).await;
        }

        let page: Vec<PhotoMeta> = all.into_iter()
            .skip(offset)
            .take(limit)
            .filter_map(|path| {
                let id = path.to_string_lossy().to_string();
                let filename = path.file_name()?.to_string_lossy().to_string();
                Some(PhotoMeta {
                    id:           id.clone(),
                    filename,
                    width:        0,
                    height:       0,
                    taken_at:     None,
                    download_url: Some(format!("file://{}", id)),
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
        // id is the absolute path for local files
        let path = PathBuf::from(&meta.id);
        let bytes = fs::read(&path).await
            .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
        debug!("Read {} bytes from {}", bytes.len(), path.display());
        Ok(bytes)
    }
}
