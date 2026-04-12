/// Google Drive plugin for PicoGallery — rclone backend.
///
/// Uses Google Drive API (unrestricted, `drive.readonly` scope) as the photo source,
/// since the Google Photos Library API was permanently removed on March 31, 2025.
///
/// On first run, spawns `rclone authorize "drive"` which opens a browser for a
/// one-time Google sign-in (uses rclone's own verified OAuth app — no Google Cloud
/// project or API key required).  The resulting token is stored in
/// `<config_dir>/picogallery/rclone-gdrive.conf`.  Every subsequent run reuses
/// that token; rclone refreshes it automatically.
///
/// After auth, image files are synced from Google Drive to `sync_dir` via
/// `rclone copy` and served from disk.
///
/// Config keys (in config.toml):
///   sync_dir      — local cache directory    (default: /tmp/picogallery-gdrive)
///   drive_folder  — Drive subfolder to sync  (default: "" = root)
///   max_transfer  — MB cap per sync run      (default: "500")
use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{debug, info, warn};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

/// Name of the rclone remote written into our private config file.
const REMOTE: &str = "picogallery-gdrive";

pub struct GooglePhotosPlugin {
    cfg:       PluginConfig,
    conf_path: PathBuf,  // our generated rclone.conf (isolated from user's rclone)
}

impl GooglePhotosPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        let conf_path = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("picogallery")
            .join("rclone-gdrive.conf");
        Self { cfg, conf_path }
    }

    // ── Config helpers ────────────────────────────────────────────────────────

    fn sync_dir(&self) -> PathBuf {
        PathBuf::from(self.cfg.get_str("sync_dir").unwrap_or("/tmp/picogallery-gdrive"))
    }

    fn rclone_src(&self) -> String {
        format!("{}:", REMOTE)
    }

    /// Returns --drive-root-folder-id args if drive_folder_id is set in config.
    fn drive_root_args(&self) -> Vec<String> {
        match self.cfg.get_str("drive_folder_id") {
            Some(id) if !id.is_empty() => vec!["--drive-root-folder-id".to_string(), id.to_string()],
            _ => vec![],
        }
    }

    fn max_transfer_mb(&self) -> String {
        format!("{}M", self.cfg.get_str("max_transfer").unwrap_or("500"))
    }

    // ── Auth ──────────────────────────────────────────────────────────────────

    fn token_saved(&self) -> bool { self.conf_path.exists() }

    /// Run `rclone authorize "drive" --drive-scope drive.readonly`, parse the printed
    /// token, and write a minimal rclone config that subsequent `rclone copy` calls use.
    async fn run_rclone_authorize(&self) -> Result<()> {
        info!("Google Drive: starting rclone authorize — browser will open…");
        println!("\n=== Google Drive — one-time sign-in ===");
        println!("A browser window is opening. Sign in to Google and approve access.");
        println!("(On a headless Pi, visit the printed URL from another device.)\n");

        let output = Command::new("rclone")
            .args(["authorize", "drive", "--drive-scope", "drive.readonly"])
            .output()
            .await
            .context("running 'rclone authorize drive' — is rclone installed?")?;

        // rclone prints the token to stdout between markers; some versions use stderr.
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        debug!("rclone authorize output:\n{}", combined);

        let token_json = extract_token_json(&combined).with_context(|| {
            format!(
                "Could not find token in rclone authorize output.\n\
                 Full output:\n{}",
                combined
            )
        })?;

        // Write a self-contained rclone config with the token embedded.
        // Using --config <this_file> keeps us isolated from any existing rclone setup.
        let conf = format!(
            "[{}]\ntype = drive\nscope = drive.readonly\ntoken = {}\n",
            REMOTE, token_json
        );
        fs::create_dir_all(self.conf_path.parent().unwrap()).await?;
        fs::write(&self.conf_path, conf).await
            .with_context(|| format!("writing {}", self.conf_path.display()))?;

        info!("Google Drive: token saved → {}", self.conf_path.display());
        Ok(())
    }

    // ── Sync ──────────────────────────────────────────────────────────────────

    /// Foreground sync: download a batch of images quickly so the slideshow can start.
    async fn sync_initial(&self) -> Result<()> {
        let conf    = self.conf_path.to_str().unwrap_or("");
        let dst     = self.sync_dir();
        let dst_str = dst.to_str().unwrap_or("");
        let src     = self.rclone_src();

        info!("Google Drive: initial sync from {} (up to 100 MB)…", src);

        let root_args = self.drive_root_args();
        let mut cmd_args = vec![
            "--config".to_string(), conf.to_string(),
            "copy".to_string(),     src.clone(),
            dst_str.to_string(),
            "--max-transfer".to_string(), "100M".to_string(),
            "--transfers".to_string(),    "4".to_string(),
            "--include".to_string(),      "*.{jpg,jpeg,png,gif,webp,JPG,JPEG,PNG,GIF,WEBP}".to_string(),
        ];
        cmd_args.extend(root_args);

        let out = Command::new("rclone")
            .args(&cmd_args)
            .output()
            .await
            .context("rclone initial sync (google drive)")?;

        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() && !stderr.contains("Max transfer") {
            warn!("rclone initial sync: {}", stderr.trim());
        }

        let found = Self::list_local_images(&dst).await;
        if found.is_empty() {
            warn!("Google Drive: no images found after initial sync. Check drive_folder config.");
        } else {
            info!("Google Drive: initial sync done — {} images.", found.len());
        }
        Ok(())
    }

    /// Background sync: keep pulling images from Drive without blocking.
    async fn spawn_sync(&self) {
        let conf      = self.conf_path.clone();
        let src       = self.rclone_src();
        let dst       = self.sync_dir();
        let max_mb    = self.max_transfer_mb();
        let root_args = self.drive_root_args();

        tokio::spawn(async move {
            info!("rclone background sync: {} → {}", src, dst.display());
            let mut args = vec![
                "--config".to_string(),       conf.to_str().unwrap_or("").to_string(),
                "copy".to_string(),           src.clone(),
                dst.to_str().unwrap_or("").to_string(),
                "--max-transfer".to_string(), max_mb,
                "--transfers".to_string(),    "2".to_string(),
                "--checkers".to_string(),     "4".to_string(),
                "--no-traverse".to_string(),
                "--include".to_string(),      "*.{jpg,jpeg,png,gif,webp,JPG,JPEG,PNG,GIF,WEBP}".to_string(),
            ];
            args.extend(root_args);
            let result = Command::new("rclone")
                .args(&args)
                .output()
                .await;

            match result {
                Ok(o) if o.status.success() => info!("rclone background sync complete."),
                Ok(o) => warn!(
                    "rclone sync exited {}: {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => warn!("rclone spawn error: {}", e),
            }
        });
    }

    // ── Local file listing ────────────────────────────────────────────────────

    async fn list_local_images(dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let Ok(mut entries) = fs::read_dir(dir).await else { return paths };
        let mut stack = vec![entries];

        while let Some(rd) = stack.last_mut() {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let path = entry.path();
                    match entry.file_type().await {
                        Ok(ft) if ft.is_dir() => {
                            if let Ok(sub) = fs::read_dir(&path).await {
                                stack.push(sub);
                            }
                        }
                        Ok(_) if is_image(&path) => paths.push(path),
                        _ => {}
                    }
                }
                Ok(None) | Err(_) => { stack.pop(); }
            }
        }
        paths
    }
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for GooglePhotosPlugin {
    fn name(&self)         -> &str { "google-photos" }
    fn display_name(&self) -> &str { "Google Drive (Photos)"  }
    fn version(&self)      -> &str { "0.4.0"                  }

    async fn init(&mut self, _cfg: &PluginConfig) -> Result<()> {
        fs::create_dir_all(self.sync_dir()).await
            .with_context(|| format!("creating sync_dir {}", self.sync_dir().display()))?;
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        if self.token_saved() { AuthStatus::Authenticated } else { AuthStatus::NotAuthenticated }
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        // Check rclone is present.
        let rclone_ok = Command::new("rclone").arg("version").output().await
            .map(|o| o.status.success()).unwrap_or(false);

        if !rclone_ok {
            return Ok(AuthStatus::PendingUserAction {
                message: "rclone is not installed.\n\
                          macOS: brew install rclone\n\
                          Pi:    sudo apt install rclone\n\
                          Then restart picogallery — it will sign in automatically.".to_string(),
                poll_interval_secs: 10,
            });
        }

        // Already have a saved token — nothing to do.
        if self.token_saved() {
            info!("Google Drive: using saved rclone token.");
            return Ok(AuthStatus::Authenticated);
        }

        // First run: do the one-time browser sign-in.
        self.run_rclone_authorize().await?;
        Ok(AuthStatus::Authenticated)
    }

    async fn refresh_auth(&mut self) -> Result<()> { Ok(()) } // rclone handles token refresh

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let local = Self::list_local_images(&self.sync_dir()).await;

        if local.is_empty() {
            // First run: foreground sync so the slideshow has something to show immediately.
            self.sync_initial().await?;
        }

        // Kick off one background sync pass (only on first page).
        if offset == 0 {
            self.spawn_sync().await;
        }

        let paths = Self::list_local_images(&self.sync_dir()).await;

        if paths.is_empty() {
            warn!("Google Drive: still no images after initial sync. Check drive_folder_id in config.");
            return Ok(vec![]);
        }

        let photos: Vec<PhotoMeta> = paths.into_iter()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, path)| {
                let filename = path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                PhotoMeta {
                    id:           (offset + i).to_string(),
                    filename,
                    width:        0,
                    height:       0,
                    taken_at:     None,
                    download_url: Some(path.to_string_lossy().to_string()),
                    extra:        Default::default(),
                }
            })
            .collect();

        info!("Google Drive: {} photos at offset {}.", photos.len(), offset);
        Ok(photos)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _dw: u32,
        _dh: u32,
    ) -> Result<Vec<u8>> {
        let path = meta.download_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("no local path for {}", meta.filename))?;
        fs::read(path).await
            .with_context(|| format!("reading local photo {}", path))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the token JSON that rclone prints between its paste markers.
/// Handles both stdout and stderr across rclone versions.
fn extract_token_json(output: &str) -> Option<String> {
    // Primary: look for content between "--->" and "<---End paste"
    if let Some(arrow) = output.find("--->") {
        let after = &output[arrow + 4..];
        let end   = after.find("<---").unwrap_or(after.len());
        let candidate = after[..end].trim();
        if candidate.starts_with('{') && candidate.ends_with('}') {
            return Some(candidate.to_string());
        }
    }

    // Fallback: find any JSON object containing both access_token and refresh_token.
    let bytes = output.as_bytes();
    let mut depth = 0usize;
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => {
                if depth == 0 { start = Some(i); }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let candidate = &output[s..=i];
                        if candidate.contains("access_token") && candidate.contains("refresh_token") {
                            return Some(candidate.to_string());
                        }
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    None
}

fn is_image(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "webp")
    )
}
