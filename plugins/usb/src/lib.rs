/// USB auto-mount and scan plugin for PicoGallery.
///
/// Automatically detects USB storage devices (sda1, sdb1, etc.) on local network/OS,
/// mounts them using udisksctl or mount, and recursively scans them for JPEGs.
use anyhow::Result;
use async_trait::async_trait;
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration};

use picogallery_core::{
    exif_thumb_from_head, AuthStatus, FetchIntent, PhotoMeta, PhotoPlugin, PluginConfig,
    EXIF_HEAD_SCAN_BYTES,
};

pub struct UsbPlugin {
    _cfg: PluginConfig,
    photos: Arc<RwLock<Vec<PathBuf>>>,
    active_mounts: Arc<Mutex<HashMap<String, (PathBuf, bool)>>>, // partition -> (mount_path, mounted_by_us)
    ready: Arc<Notify>,
    poller: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl UsbPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            _cfg: cfg,
            photos: Arc::new(RwLock::new(Vec::new())),
            active_mounts: Arc::new(Mutex::new(HashMap::new())),
            ready: Arc::new(Notify::new()),
            poller: std::sync::Mutex::new(None),
        }
    }
}

fn is_image(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg")
    )
}

const MAX_DIRS: usize = 10_000;
const MAX_FILES: usize = 100_000;

async fn scan_dir(dir: &Path, root: &Path, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let dir = fs::canonicalize(dir)
        .await
        .unwrap_or_else(|_| dir.to_path_buf());
    let root = fs::canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    scan_dir_inner(&dir, &root, visited, out).await;
}

async fn scan_dir_inner(
    dir: &Path,
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    if visited.len() >= MAX_DIRS || out.len() >= MAX_FILES {
        return;
    }
    if !visited.insert(dir.to_path_buf()) {
        return;
    }
    let mut rd = match fs::read_dir(dir).await {
        Ok(r) => r,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let canonical = match fs::canonicalize(&path).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !canonical.starts_with(root) {
            warn!(
                "USB plugin: rejecting path outside mount {}: {}",
                root.display(),
                canonical.display()
            );
            continue;
        }
        if canonical.to_str().is_none() {
            warn!(
                "USB plugin: skipping non-UTF-8 path {}",
                canonical.display()
            );
            continue;
        }
        let is_dir = fs::metadata(&canonical)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            Box::pin(scan_dir_inner(&canonical, root, visited, out)).await;
        } else if is_image(&canonical) && out.len() < MAX_FILES {
            out.push(canonical);
        }
    }
}

async fn get_partitions() -> Vec<String> {
    let mut parts = Vec::new();
    if let Ok(content) = fs::read_to_string("/proc/partitions").await {
        for line in content.lines() {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() == 4 {
                let name = tokens[3];
                // Match partition names like sda1, sdb2, sdc1 (sd[a-z][0-9]+)
                if name.starts_with("sd") && name.chars().nth(3).is_some_and(|c| c.is_ascii_digit())
                {
                    parts.push(name.to_string());
                }
            }
        }
    }
    parts
}

fn unescape_fstab(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

async fn get_existing_mount(partition: &str) -> Option<PathBuf> {
    let device_path = format!("/dev/{}", partition);
    if let Ok(content) = fs::read_to_string("/proc/mounts").await {
        for line in content.lines() {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() >= 2 && tokens[0] == device_path {
                return Some(PathBuf::from(unescape_fstab(tokens[1])));
            }
        }
    }
    None
}

async fn mount_partition(partition: &str) -> Option<PathBuf> {
    // 1. Try udisksctl first (non-root auto mount)
    let output = tokio::process::Command::new("udisksctl")
        .args(["mount", "-b", &format!("/dev/{}", partition)])
        .output()
        .await;
    if let Ok(out) = output {
        if out.status.success() {
            let msg = String::from_utf8_lossy(&out.stdout);
            if let Some(pos) = msg.find(" at ") {
                let path_str = msg[pos + 4..].trim().trim_end_matches('.');
                return Some(PathBuf::from(path_str));
            }
        }
    }

    // 2. Fallback to raw mount command
    let mount_dir = PathBuf::from(format!("/media/picogallery-usb-{}", partition));
    let _ = fs::create_dir_all(&mount_dir).await;
    let output = tokio::process::Command::new("mount")
        .args([
            "-o",
            "ro",
            &format!("/dev/{}", partition),
            &mount_dir.to_string_lossy(),
        ])
        .output()
        .await;
    if let Ok(out) = output {
        if out.status.success() {
            return Some(mount_dir);
        }
    }
    None
}

async fn unmount_partition(partition: &str) {
    let _ = tokio::process::Command::new("udisksctl")
        .args(["unmount", "-b", &format!("/dev/{}", partition)])
        .output()
        .await;
    let mount_dir = format!("/media/picogallery-usb-{}", partition);
    let _ = tokio::process::Command::new("umount")
        .arg(&mount_dir)
        .output()
        .await;
    let _ = fs::remove_dir(&mount_dir).await;
}

async fn run_usb_poller(
    photos: Arc<RwLock<Vec<PathBuf>>>,
    active_mounts: Arc<Mutex<HashMap<String, (PathBuf, bool)>>>,
    ready: Arc<Notify>,
) {
    let mut first_scan = true;
    loop {
        let current_partitions = get_partitions().await;
        let mut mounts = active_mounts.lock().await;

        // 1. Detect removals
        let mut removed = Vec::new();
        for key in mounts.keys() {
            if !current_partitions.contains(key) {
                removed.push(key.clone());
            }
        }
        let mut list_changed = false;
        for key in &removed {
            if let Some((path, mounted_by_us)) = mounts.remove(key) {
                info!(
                    "USB partition removed: {} (from path: {})",
                    key,
                    path.display()
                );
                if mounted_by_us {
                    unmount_partition(key).await;
                }
                list_changed = true;
            }
        }

        // 2. Detect insertions
        for part in &current_partitions {
            if !mounts.contains_key(part) {
                info!("New USB partition detected: {}", part);
                if let Some(existing) = get_existing_mount(part).await {
                    info!(
                        "USB partition {} is already mounted at: {}",
                        part,
                        existing.display()
                    );
                    mounts.insert(part.clone(), (existing, false));
                    list_changed = true;
                } else if let Some(new_mount) = mount_partition(part).await {
                    info!(
                        "Successfully mounted USB partition {} at: {}",
                        part,
                        new_mount.display()
                    );
                    mounts.insert(part.clone(), (new_mount, true));
                    list_changed = true;
                } else {
                    warn!("Could not mount USB partition {}", part);
                }
            }
        }

        // 3. Re-scan if changes happened. Drop the mount lock first so
        // get_photo_bytes waiters are not stalled for the whole walk.
        if list_changed {
            let roots: Vec<PathBuf> = mounts.values().map(|(path, _)| path.clone()).collect();
            drop(mounts);
            let mut all_photos = Vec::new();
            let mut visited = HashSet::new();
            for path in &roots {
                scan_dir(path, path, &mut visited, &mut all_photos).await;
            }
            all_photos.sort();
            info!("USB scan complete. Found {} photos.", all_photos.len());
            *photos.write().await = all_photos;
        } else {
            drop(mounts);
        }
        if first_scan {
            first_scan = false;
            // `notify_one` stores a permit if list_photos has not started
            // waiting yet, closing the init/list startup race.
            ready.notify_one();
        }
        sleep(Duration::from_secs(5)).await;
    }
}

#[async_trait]
impl PhotoPlugin for UsbPlugin {
    fn name(&self) -> &str {
        "usb"
    }

    fn uses_engine_image_cache(&self) -> bool {
        false
    }

    fn display_name(&self) -> &str {
        "USB Auto-Mount"
    }

    async fn init(&mut self, config: &PluginConfig) -> Result<()> {
        self._cfg = config.clone();
        if self
            .poller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return Ok(());
        }
        let photos = self.photos.clone();
        let active_mounts = self.active_mounts.clone();
        let ready = self.ready.clone();
        let task = tokio::spawn(async move {
            run_usb_poller(photos, active_mounts, ready).await;
        });
        *self.poller.lock().unwrap_or_else(|e| e.into_inner()) = Some(task.abort_handle());
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::Authenticated
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        Ok(AuthStatus::Authenticated)
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        if self.photos.read().await.is_empty() {
            let _ = tokio::time::timeout(Duration::from_secs(10), self.ready.notified()).await;
        }
        let photos = self.photos.read().await;
        let mut page = Vec::new();
        for path in photos.iter().skip(offset).take(limit) {
            let Some(id) = path.to_str() else {
                continue;
            };
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(id)
                .to_string();
            let taken_at = file_mtime_utc(path).await;
            page.push(PhotoMeta {
                id: id.to_string(),
                filename,
                width: 0,
                height: 0,
                taken_at,
                download_url: None, // read directly from disk
                album: None,
                title: None,
                location: None,
                is_favorite: false,
                extra: Default::default(),
            });
        }
        Ok(page)
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some(handle) = self.poller.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
        Ok(())
    }

    async fn get_photo_bytes(&self, meta: &PhotoMeta, intent: FetchIntent) -> Result<Vec<u8>> {
        const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
        let path = PathBuf::from(&meta.id);
        let canonical = fs::canonicalize(&path)
            .await
            .map_err(|e| anyhow::anyhow!("resolving path {}: {}", path.display(), e))?;

        let mounts = self.active_mounts.lock().await;
        let allowed = mounts.values().any(|(root, _)| canonical.starts_with(root));
        if !allowed {
            return Err(anyhow::anyhow!(
                "security: {} is outside active USB mounts",
                canonical.display()
            ));
        }
        drop(mounts);

        let file_meta = fs::metadata(&canonical).await?;
        if file_meta.len() > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "image too large ({} MB): {}",
                file_meta.len() / 1_048_576,
                meta.filename
            ));
        }

        if let Some(thumb) = try_gallery_exif_thumb(&canonical, file_meta.len(), intent).await? {
            return Ok(thumb);
        }

        let bytes = fs::read(&canonical).await?;
        if bytes.len() < 3 || bytes[0] != 0xFF || bytes[1] != 0xD8 || bytes[2] != 0xFF {
            return Err(anyhow::anyhow!("not a JPEG (bad magic): {}", meta.filename));
        }
        Ok(bytes)
    }
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
    if head.len() < 3 || head[0] != 0xFF || head[1] != 0xD8 || head[2] != 0xFF {
        return Ok(None);
    }
    Ok(exif_thumb_from_head(&head, intent.target_edge()))
}

async fn file_mtime_utc(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| chrono::DateTime::<chrono::Utc>::from_timestamp(d.as_secs() as i64, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
    fn test_unescape_fstab() {
        assert_eq!(unescape_fstab(r"hello\040world"), "hello world");
        assert_eq!(unescape_fstab(r"tab\011here"), "tab\there");
        assert_eq!(unescape_fstab(r"new\012line"), "new\nline");
        assert_eq!(unescape_fstab(r"back\134slash"), "back\\slash");
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

        let mut visited = HashSet::new();
        let mut out = Vec::new();
        scan_dir(root, root, &mut visited, &mut out).await;

        assert_eq!(out.len(), 2);
        let out_paths: Vec<_> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(out_paths.contains(&"1.jpg".to_string()));
        assert!(out_paths.contains(&"2.jpg".to_string()));
    }

    #[tokio::test]
    async fn test_auth_status() {
        let mut plugin = UsbPlugin::new(PluginConfig::default());
        assert_eq!(plugin.auth_status().await, AuthStatus::Authenticated);
        assert_eq!(
            plugin.authenticate().await.unwrap(),
            AuthStatus::Authenticated
        );
    }

    #[tokio::test]
    async fn test_list_photos_and_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let f1 = root.join("1.jpg");
        fs::write(&f1, [0xFF, 0xD8, 0xFF, 0x12, 0x34])
            .await
            .unwrap();

        let plugin = UsbPlugin::new(PluginConfig::default());
        *plugin.photos.write().await = vec![f1.clone()];

        let meta = plugin.list_photos(10, 0).await.unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].filename, "1.jpg");

        // Try getting bytes when not in active mounts - should fail
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
        assert!(err.to_string().contains("outside active USB mounts"));

        // Add to active mounts
        plugin.active_mounts.lock().await.insert(
            "sda1".to_string(),
            (fs::canonicalize(root).await.unwrap(), true),
        );
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
}
