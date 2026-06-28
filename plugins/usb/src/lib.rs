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
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

pub struct UsbPlugin {
    _cfg: PluginConfig,
    photos: Arc<RwLock<Vec<PathBuf>>>,
    active_mounts: Arc<Mutex<HashMap<String, (PathBuf, bool)>>>, // partition -> (mount_path, mounted_by_us)
}

impl UsbPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            _cfg: cfg,
            photos: Arc::new(RwLock::new(Vec::new())),
            active_mounts: Arc::new(Mutex::new(HashMap::new())),
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

async fn scan_dir(dir: &Path, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
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
        let is_dir = fs::metadata(&canonical)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            Box::pin(scan_dir(&canonical, visited, out)).await;
        } else if is_image(&canonical) {
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
                if name.starts_with("sd") && name.chars().nth(3).is_some_and(|c| c.is_ascii_digit()) {
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
        .args(["-o", "ro", &format!("/dev/{}", partition), &mount_dir.to_string_lossy()])
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
) {
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
                info!("USB partition removed: {} (from path: {})", key, path.display());
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
                    info!("USB partition {} is already mounted at: {}", part, existing.display());
                    mounts.insert(part.clone(), (existing, false));
                    list_changed = true;
                } else if let Some(new_mount) = mount_partition(part).await {
                    info!("Successfully mounted USB partition {} at: {}", part, new_mount.display());
                    mounts.insert(part.clone(), (new_mount, true));
                    list_changed = true;
                } else {
                    warn!("Could not mount USB partition {}", part);
                }
            }
        }

        // 3. Re-scan if changes happened
        if list_changed {
            let mut all_photos = Vec::new();
            for (path, _) in mounts.values() {
                let mut visited = HashSet::new();
                scan_dir(path, &mut visited, &mut all_photos).await;
            }
            all_photos.sort();
            info!("USB scan complete. Found {} photos.", all_photos.len());
            *photos.write().await = all_photos;
        }

        drop(mounts);
        sleep(Duration::from_secs(5)).await;
    }
}

#[async_trait]
impl PhotoPlugin for UsbPlugin {
    fn name(&self) -> &str {
        "usb"
    }

    fn display_name(&self) -> &str {
        "USB Auto-Mount"
    }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        let photos = self.photos.clone();
        let active_mounts = self.active_mounts.clone();
        tokio::spawn(async move {
            run_usb_poller(photos, active_mounts).await;
        });
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::Authenticated
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        Ok(AuthStatus::Authenticated)
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let photos = self.photos.read().await;
        let page: Vec<PhotoMeta> = photos
            .iter()
            .skip(offset)
            .take(limit)
            .map(|path| {
                let id = path.to_string_lossy().to_string();
                let filename = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                PhotoMeta {
                    id,
                    filename,
                    width: 0,
                    height: 0,
                    taken_at: None,
                    download_url: None, // read directly from disk
                    extra: Default::default(),
                }
            })
            .collect();
        Ok(page)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _display_width: u32,
        _display_height: u32,
    ) -> Result<Vec<u8>> {
        let bytes = fs::read(&meta.id).await?;
        Ok(bytes)
    }
}
