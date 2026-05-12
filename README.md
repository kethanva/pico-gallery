# PicoGallery

> Lightweight, plugin-based photo slideshow for Raspberry Pi — no desktop environment required.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.75+-orange)
![Platform](https://img.shields.io/badge/Platform-Raspberry%20Pi%20Zero%2F1%2F2%2F3%2F4-red)
[![Build & Release](https://github.com/kethanva/pico-gallery/actions/workflows/release.yml/badge.svg)](https://github.com/kethanva/pico-gallery/actions/workflows/release.yml)

---

## What's new

| Version | Feature |
|---------|---------|
| **current** | **WebDAV/Nextcloud plugin** — sync photos from Nextcloud, Synology, ownCloud, or any WebDAV server. No shell tools; pure Rust. Works offline after first sync. |
| **current** | **Display scheduling** — configure `on_time`/`off_time` to cut HDMI power at night automatically. Inactive by default; opt-in per-frame. |
| 0.0.16 | Cross-fade, slide, and cut transitions; async prefetch; LRU disk cache |
| 0.0.16 | Directory plugin with album support; Google Drive; Amazon Photos |

---

## Choosing a photo source

| Plugin | Best for | Requires |
|--------|----------|---------|
| **`directory`** ★ default | USB drive, local folder, NAS mount | Nothing extra |
| **`webdav`** ★ recommended for families | Nextcloud, Synology, ownCloud — add photos from your phone | Network access |
| `google-photos` | Google Drive folder | rclone |
| `amazon-photos` | Amazon Photos library | LWA developer app |
| `local` | Multiple root paths | Nothing extra |

**TL;DR for a new frame:**
- Easiest: copy JPEGs to an SD card or USB drive → use the `directory` plugin.
- Best for sharing with family: set up Nextcloud on a home server or a VPS → use the `webdav` plugin; everyone uploads from their phone and the frame picks up new photos automatically.

---

## Installation on Raspberry Pi

### One-line installer (recommended)

No Rust toolchain, no compilation — downloads the pre-built binary and configures everything automatically.

```bash
curl -sSL https://raw.githubusercontent.com/kethanva/pico-gallery/main/install.sh | bash
```

Pin a specific version:

```bash
PICOGALLERY_VERSION=v0.1.0 bash <(curl -sSL https://raw.githubusercontent.com/kethanva/pico-gallery/main/install.sh)
```

Force build from source (skips binary download):

```bash
PICOGALLERY_BUILD=1 bash <(curl -sSL https://raw.githubusercontent.com/kethanva/pico-gallery/main/install.sh)
```

**What the installer does (zero human intervention):**

| Step | Action |
|------|--------|
| 1 | Detects architecture (`aarch64` or `armv7`) |
| 2 | Tries to download a pre-built binary from GitHub Releases |
| 3 | If no release exists, automatically falls back to cloning and building from source |
| 4 | Verifies the SHA-256 checksum (download mode) |
| 5 | Installs runtime dependencies: `libsdl2-2.0-0`, `libdrm2`, `ca-certificates`, `rclone` |
| 6 | Installs the binary to `/usr/local/bin/picogallery` |
| 7 | Adds your user to the `video`, `render`, and `input` groups |
| 8 | Writes a default config to `~/.config/picogallery/config.toml` |
| 9 | Installs and enables a systemd service (`picogallery.service`) |
| 10 | Sets `gpu_mem=64` in `/boot/config.txt` (or `/boot/firmware/config.txt` on Bookworm) |

**Install modes:**

| Mode | When | What happens |
|------|------|-------------|
| Download (fast) | A GitHub Release with artifacts exists | Downloads ~4 MB binary, no Rust needed |
| Build (fallback) | No release yet, or `PICOGALLERY_BUILD=1` | Installs Rust + build deps, compiles from source (~10 min on Pi 4) |

After installation:

```bash
# 1. Edit your photo source
nano ~/.config/picogallery/config.toml

# 2. Test interactively (Linux console)
SDL_VIDEODRIVER=kmsdrm picogallery

# 2b. Test interactively (macOS / Linux Desktop)
picogallery

# 3. Start the service
sudo systemctl start picogallery

# 4. Watch logs
sudo journalctl -u picogallery -f

# 5. Reboot to apply GPU memory and group changes
sudo reboot
```

### Supported architectures

| Archive | Architecture | Devices |
|---------|-------------|---------|
| `*-linux-aarch64.tar.gz` | 64-bit ARM | Pi Zero 2 W, Pi 3, Pi 4, Pi 5 (64-bit OS) |
| `*-linux-armv7.tar.gz` | 32-bit ARM | Pi 2, Pi 3, Pi 4 (32-bit OS) |

---

## Default plugin — `directory`

Out of the box PicoGallery points at `~/Pictures/PicoGallery` on the Pi. Drop any JPEG, PNG, WebP, or GIF into that folder (or a sub-folder — each sub-folder becomes an "album") and the frame picks it up on the next restart.

```bash
# Create the default folder and copy some photos
mkdir -p ~/Pictures/PicoGallery
scp *.jpg pi@raspberrypi.local:~/Pictures/PicoGallery/
sudo systemctl restart picogallery
```

Sub-folders work as albums:

```
~/Pictures/PicoGallery/
├── Vacation 2024/
│   ├── beach.jpg
│   └── sunset.jpg
└── Birthday/
    └── cake.jpg
```

To change the source folder, edit `~/.config/picogallery/config.toml`:

```toml
[[plugins]]
name    = "directory"
enabled = true
path    = "/mnt/usb/photos"   # USB drive, NAS mount, etc.
order   = "shuffle"           # "shuffle" | "alphabetical" | "date_modified"
recursive = true
```

---

## WebDAV / Nextcloud plugin

The WebDAV plugin lets the frame pull photos directly from any WebDAV server — Nextcloud, ownCloud, Synology DSM, or a plain Apache/nginx WebDAV share. Photos are synced to local disk on startup and served offline. The background sync loop fetches new photos hourly so the frame stays current without any manual intervention.

This is the same model used by [photOS](https://github.com/avanc/photOS) (davfs2 + rsync) but implemented entirely in Rust — no shell tools, no mounted filesystems, no external binaries.

### End-to-end: Nextcloud setup

**On the server (one time):**

1. Install Nextcloud on a home server, VPS, or NAS. The [All-in-One installer](https://github.com/nextcloud/all-in-one) takes about 10 minutes on a Debian/Ubuntu box.
2. Create a user for the frame, or use an existing account.
3. In Nextcloud → **Settings → Security → Devices & sessions**, generate an **App Password** — use that instead of your main password.

**Finding your WebDAV URL:**

In Nextcloud, go to **Files → ⋯ (top-right) → WebDAV**. The URL looks like:

```
https://cloud.example.com/remote.php/dav/files/YOUR_USERNAME
```

For Synology DSM it is:

```
https://nas.local:5006/photo          # DSM Photo Station
https://nas.local:5006/home/Photos    # DSM personal folder
```

For ownCloud it is:

```
https://cloud.example.com/remote.php/webdav
```

**Configure PicoGallery:**

```toml
[[plugins]]
name     = "webdav"
enabled  = true

# Full WebDAV endpoint URL (required)
url      = "https://cloud.example.com/remote.php/dav/files/YOUR_USERNAME"

username = "your-username"
password = "your-app-password"    # app password, not your Nextcloud login password

# Sub-folder on the server to sync (default: "/" — everything under url)
remote_path = "/Photos"

# Local cache on the Pi (created automatically)
sync_dir = "/tmp/picogallery-webdav"

# Re-sync every hour in the background (0 = startup only)
sync_interval_secs = 3600

# Uncomment for self-signed certs (local NAS without a valid cert)
# skip_tls_verify = true
```

Restart the service:

```bash
sudo systemctl restart picogallery
sudo journalctl -u picogallery -f   # watch the initial sync progress
```

### How to transfer photos to the frame

Once the WebDAV plugin is running, any of these methods add photos to the frame automatically on the next sync:

| Method | Instructions |
|--------|-------------|
| **Nextcloud mobile app** | Install on iOS/Android → sign in → upload to the `Photos` folder |
| **Nextcloud web** | Open `cloud.example.com` in a browser → drag files into the `Photos` folder |
| **Nextcloud desktop sync** | Install on any PC/Mac; add the `Photos` folder to your sync |
| **WebDAV from Finder (macOS)** | Go → Connect to Server → paste your WebDAV URL → drag photos in |
| **WebDAV from Windows** | Map a network drive using the WebDAV URL → copy photos in |
| **rclone (advanced)** | `rclone copy ~/Pictures/ nextcloud:Photos/` |

New photos appear on the frame within `sync_interval_secs` (default 1 hour) without touching the Pi.

### Offline operation

After the first sync, the frame shows photos from the local `sync_dir` cache. A network outage — or taking the frame to a location with no WiFi — does not interrupt the slideshow. New photos are pulled on the next successful connection.

### Security notes

- Use an **app password** (Nextcloud → Settings → Security → App passwords), not your main account password. App passwords can be revoked individually if the Pi is lost or stolen.
- Set file permissions on the config so only your user can read it: `chmod 600 ~/.config/picogallery/config.toml`.
- For an internal NAS with a self-signed certificate, set `skip_tls_verify = true`. Do not use this option for servers reachable over the internet.

---

## Display scheduling

Keep the screen off at night without touching the Pi. Both fields are required; omitting them (the default) keeps the display on at all times.

```toml
[display]
slide_duration_secs = 10
transition          = "fade"

# Optional: automatic HDMI on/off schedule (local time, 24-hour HH:MM)
# Both fields must be set to activate scheduling.
on_time  = "07:00"   # display turns on at 7 am
off_time = "22:00"   # display turns off at 10 pm
```

**What happens when the schedule fires:**

1. At `off_time` the renderer blanks the screen to solid black.
2. On Raspberry Pi, `vcgencmd display_power 0` is called — the HDMI signal is cut so the monitor enters standby and draws near-zero power.
3. At `on_time` the HDMI signal is restored (`vcgencmd display_power 1`) and the slideshow resumes immediately with the next photo.

On non-Pi Linux and macOS the HDMI call is a silent no-op and the black screen is the sole power-saving mechanism (useful for development/testing).

**Overnight windows** (display on at night, off during the day) are also supported:

```toml
on_time  = "20:00"
off_time = "08:00"   # active 20:00–08:00, off 08:00–20:00
```

Times are interpreted in the system's **local time zone**.

---

## Features

- **No X11 / no desktop** — renders directly to the KMS/DRM framebuffer via SDL2.
- **Plugin architecture** — Google Drive, Amazon Photos, local directory/filesystem, WebDAV/Nextcloud; add your own with one Rust trait.
- **WebDAV / Nextcloud plugin** — sync from Nextcloud, Synology, ownCloud, or any WebDAV server. Pure Rust. Works offline after first sync.
- **Display scheduling** — configure `on_time`/`off_time` to cut HDMI power automatically. Opt-in; off by default.
- **Google Drive via rclone** — `drive.readonly` scope, no Google Cloud project or API key needed.
- **One-time sign-in** — browser opens automatically on first run; token is saved and reused forever.
- **Disk cache with LRU eviction** — photos survive reboots and WiFi outages.
- **Background prefetch** — next N photos are fetched while the current one displays.
- **Cross-fade / slide / cut transitions** — configurable in `config.toml`.
- **Keyboard control** — `→`/`Space` next, `←` prev, `P` pause, `Q`/`Esc` quit.

---

## Hardware requirements

| Device | Notes |
|---|---|
| Raspberry Pi Zero W / 2W | Tested; use `--jobs 1` when cross-compiling |
| Raspberry Pi 2 / 3 / 4 | Full speed |

**Required**: display connected before boot (HDMI or DSI).
**Not required**: keyboard, mouse, X server, desktop environment.

---

## System dependencies

```
libsdl2-2.0-0     SDL2 with KMS/DRM backend
libdrm2           DRM display probing (finds correct /dev/dri/cardN on Pi 4/5)
rclone            Google Drive sync only (not needed for WebDAV or local plugins)
ca-certificates   HTTPS root certs (needed for WebDAV and Google Drive)
```

---

## Quick start

### Local photos (simplest)

```bash
# 1. Create the photo folder
mkdir -p ~/Pictures/PicoGallery

# 2. Copy photos in (from a PC over SSH, or from a USB drive)
scp /path/to/photos/*.jpg pi@raspberrypi.local:~/Pictures/PicoGallery/

# 3. Config is already set up by the installer — just start
sudo systemctl start picogallery
```

### Nextcloud / WebDAV (recommended for families)

```bash
# 1. Create or edit config
nano ~/.config/picogallery/config.toml
```

Minimal config:

```toml
[display]
slide_duration_secs = 10
transition          = "fade"

[cache]
max_mb = 256

[[plugins]]
name        = "webdav"
enabled     = true
url         = "https://cloud.example.com/remote.php/dav/files/YOUR_USERNAME"
username    = "your-username"
password    = "your-app-password"
remote_path = "/Photos"
```

```bash
# 2. Restart and watch the initial sync
sudo systemctl restart picogallery
sudo journalctl -u picogallery -f
```

### Google Drive

```toml
[display]
slide_duration_secs = 10
transition          = "fade"

[cache]
max_mb = 256

[[plugins]]
name            = "google-photos"
enabled         = true
sync_dir        = "/tmp/picogallery-gdrive"
drive_folder_id = ""   # paste a Drive folder ID, or leave blank for root
```

First run opens a browser for a one-time sign-in. Token saved at `~/.config/picogallery/rclone-gdrive.conf`.

### Enable on boot (Pi)

```bash
sudo systemctl enable --now picogallery
```

---

## Configuration reference

```toml
# ─────────────────────────────────────────────────────────────────────────────
# Display
# ─────────────────────────────────────────────────────────────────────────────
[display]
slide_duration_secs = 10      # seconds per photo
transition          = "fade"  # "cut" | "fade" | "slide_left" | "slide_right"
transition_ms       = 800     # transition duration in ms (0 = instant)
fill_screen         = false   # true=crop-to-fill, false=letterbox
fps                 = 15      # max FPS (lower = less CPU on Pi Zero)
# width  = 1920               # force resolution (0 = auto-detect)
# height = 1080

# Optional display schedule — both required to activate (default: always on)
# on_time  = "07:00"          # display on  at 07:00 local time
# off_time = "22:00"          # display off at 22:00 local time

# ─────────────────────────────────────────────────────────────────────────────
# Cache
# ─────────────────────────────────────────────────────────────────────────────
[cache]
max_mb         = 256   # disk cache ceiling in MB
prefetch_count = 3     # photos to prefetch ahead (keep ≤ 3 on Pi Zero)
# dir = "/tmp/picogallery-cache"   # override cache location

# ─────────────────────────────────────────────────────────────────────────────
# Directory plugin  ★ DEFAULT — enabled out of the box
# ─────────────────────────────────────────────────────────────────────────────
[[plugins]]
name      = "directory"
enabled   = true

path      = "~/Pictures/PicoGallery"   # root folder; ~ expands to $HOME
order     = "shuffle"                  # "shuffle" | "alphabetical" | "date_modified"
recursive = true                       # include sub-folders as albums

# Limit to specific sub-folders (album names). Empty = show all albums.
# allowed_albums = ["Vacation 2024", "Family"]

# Re-scan every N seconds; 0 = startup only (default)
# rescan_interval_secs = 3600

# ─────────────────────────────────────────────────────────────────────────────
# WebDAV / Nextcloud plugin
# ─────────────────────────────────────────────────────────────────────────────
# [[plugins]]
# name     = "webdav"
# enabled  = true
# url      = "https://cloud.example.com/remote.php/dav/files/USERNAME"
# username = "alice"
# password = "your-app-password"
# remote_path      = "/Photos"               # default "/"
# sync_dir         = "/tmp/picogallery-webdav"
# sync_interval_secs = 3600                  # 0 = startup only
# skip_tls_verify  = false                   # true for self-signed certs

# ─────────────────────────────────────────────────────────────────────────────
# Google Drive plugin (requires rclone)
# ─────────────────────────────────────────────────────────────────────────────
# [[plugins]]
# name            = "google-photos"
# enabled         = true
# sync_dir        = "/tmp/picogallery-gdrive"
# drive_folder_id = ""          # Drive folder ID, or "" for root
# max_transfer    = "500"       # MB cap per sync run

# ─────────────────────────────────────────────────────────────────────────────
# Amazon Photos plugin
# ─────────────────────────────────────────────────────────────────────────────
# [[plugins]]
# name          = "amazon-photos"
# enabled       = true
# client_id     = "YOUR_LWA_CLIENT_ID"
# client_secret = "YOUR_LWA_CLIENT_SECRET"

# ─────────────────────────────────────────────────────────────────────────────
# Local filesystem plugin (multiple root paths)
# ─────────────────────────────────────────────────────────────────────────────
# [[plugins]]
# name      = "local"
# enabled   = true
# paths     = ["/mnt/usb/photos", "~/Pictures"]
# recursive = true
```

---

## Google Photos — API status (March 2025)

> **Google removed `photoslibrary.readonly` on March 31, 2025.**
> All read access to existing photo libraries via the Google Photos API is permanently blocked.
> This affects the Photos Library API, rclone's Google Photos backend, and all third-party tools.

### Recommended: Use Google Drive

PicoGallery's `google-photos` plugin now uses **Google Drive** (`drive.readonly`) as its backend, which is fully accessible and unrestricted.

**If your photos are backed up to Google Drive** (via Google Drive for Desktop / Backup & Sync):

1. Open [drive.google.com](https://drive.google.com) and navigate to the folder containing your photos
2. Copy the folder ID from the URL: `drive.google.com/drive/folders/<FOLDER_ID>`
3. Set `drive_folder_id` in `config.toml`

```toml
[[plugins]]
name             = "google-photos"
enabled          = true
sync_dir         = "/tmp/picogallery-gdrive"
drive_folder_id  = "1REc7j3GtIIzF25ARuhqnAZsayu3fRogb"   # your folder ID
```

**If your photos are only in Google Photos** (not in Drive):

Use Google Takeout to export them once, then use the local plugin:

1. Go to **[takeout.google.com](https://takeout.google.com)**
2. Deselect all → select only **Google Photos** → Next step → Create export
3. Download and extract the zip — you get a folder of JPEGs organised by date
4. Copy to your Pi and configure:

```toml
[[plugins]]
name    = "local"
enabled = true
paths   = ["/mnt/photos/Google Photos"]
```

### API status table

| Approach | Status (2026) |
|---|---|
| Google Photos Library API | Blocked since March 2025 |
| rclone Google Photos backend | Blocked (uses same API) |
| **Google Drive plugin (this app)** | **Works — `drive.readonly`, unrestricted** |
| **Google Takeout + local plugin** | **Works — recommended if photos not in Drive** |
| **WebDAV/Nextcloud plugin** | **Works — best for self-hosted photo sharing** |

---

## Running locally for development

The app runs on macOS and Linux without a Pi. SDL2 uses its native backend (Cocoa/Metal on macOS). KMS/DRM is compiled out on non-Linux platforms.

### Prerequisites

**macOS**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
brew install rclone cmake
```

**Linux**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
sudo apt-get install -y libsdl2-dev pkg-config cmake clang build-essential rclone
```

### Build

```bash
# Full build (all plugins)
cargo build

# Minimal build — directory plugin only (no rclone or network needed)
cargo build --no-default-features --features plugin-directory

# WebDAV only (no rclone needed)
cargo build --no-default-features --features plugin-webdav,plugin-directory
```

### Test with local photos

```bash
mkdir -p /tmp/picogallery-test-photos

python3 - << 'EOF'
from PIL import Image, ImageDraw
photos = [
    ("photo1.jpg", (220, 60,  60),  "Photo 1 — Red"),
    ("photo2.jpg", (60,  140, 220), "Photo 2 — Blue"),
    ("photo3.jpg", (60,  180, 80),  "Photo 3 — Green"),
    ("photo4.jpg", (200, 160, 40),  "Photo 4 — Yellow"),
    ("photo5.jpg", (140, 60,  200), "Photo 5 — Purple"),
]
for filename, colour, label in photos:
    img = Image.new("RGB", (1280, 720), colour)
    draw = ImageDraw.Draw(img)
    draw.rectangle([340, 260, 940, 460], fill=(255, 255, 255))
    draw.text((640, 360), label, fill=(30, 30, 30), anchor="mm")
    img.save(f"/tmp/picogallery-test-photos/{filename}", "JPEG", quality=90)
    print(f"Created {filename}")
EOF
```

Write a local-only config:

```bash
cat > ~/.config/picogallery/config.toml << 'EOF'
[display]
slide_duration_secs = 4
transition = "fade"
fps = 30

[cache]
max_mb = 64
prefetch_count = 2

[[plugins]]
name    = "directory"
enabled = true
path    = "/tmp/picogallery-test-photos"
order   = "alphabetical"
EOF
```

### Run (macOS)

```bash
SDL_LIB=$(find target/debug/build -name "libSDL2-2.0.0.dylib" | head -1 | xargs dirname)
DYLD_LIBRARY_PATH="$SDL_LIB" target/debug/picogallery --config ~/.config/picogallery/config.toml
```

### Test display scheduling locally

Add schedule fields to the config and check the log output:

```toml
[display]
on_time  = "00:00"   # always on during testing
off_time = "00:01"   # flip this to the next minute to watch the off-edge fire
```

---

## Project structure

```
picogallery/
├── Cargo.toml                    # workspace root + main crate (lib + bin)
├── Cargo.lock
├── install.sh                    # one-shot Pi installer
├── config.example.toml           # fully annotated config template
├── core/                         # picogallery-core: shared plugin trait
│   ├── Cargo.toml
│   └── src/lib.rs                # PhotoPlugin trait, PhotoMeta, PluginConfig
├── src/
│   ├── lib.rs                    # re-exports core + all modules
│   ├── main.rs                   # binary entry point + plugin registry
│   ├── config.rs                 # TOML config structs + display schedule logic
│   ├── cache.rs                  # LRU disk image cache
│   ├── renderer.rs               # SDL2 / KMS-DRM renderer + transitions
│   ├── display_power.rs          # vcgencmd HDMI power control (Linux/Pi)
│   └── slideshow.rs              # async slideshow engine + schedule enforcement
└── plugins/
    ├── webdav/                   # WebDAV/Nextcloud plugin (NEW)
    ├── google-photos/            # Google Drive plugin (drive.readonly via rclone)
    ├── amazon-photos/            # Amazon Photos via LWA
    ├── directory/                # Directory plugin (default; album support)
    └── local/                    # Local filesystem scanner (multiple paths)
```

---

## Why Rust? Why not Python?

| | **Rust (chosen)** | Python |
|---|---|---|
| RSS on Pi Zero | ~8 MB | ~60–120 MB |
| Binary size | ~4 MB stripped | N/A (interpreter) |
| CPU during decode | ~40% one core | ~90% one core |
| Startup time | < 0.5 s | 2–5 s |
| Packages installed | `libsdl2` + `rclone` | python3, pip, 15+ wheels |
| GC pauses during fade | None | Yes |

---

## Architecture overview

```
config.toml
    │
    ├─ [display] slide_duration, transition, schedule (on_time/off_time)
    └─ [[plugins]] ...
              │
              ▼
Plugin registry
    ├── DirectoryPlugin   (local folder + album support)        ← default
    ├── WebDavPlugin      (Nextcloud/Synology/ownCloud → disk)  ← new
    ├── GooglePhotosPlugin(Google Drive via rclone → disk)
    ├── AmazonPhotosPlugin(Amazon Photos API)
    └── LocalPlugin       (multi-path filesystem scan)
              │  dyn PhotoPlugin
              ▼
Slideshow engine
    ├─ build_queue(): list photos from all enabled plugins, shuffle
    ├─ display_loop():
    │   ├─ poll SDL2 events (Quit / Next / Prev / Pause)
    │   ├─ check display schedule → if off: black frame + vcgencmd display_power 0
    │   ├─ prefetch loop: fetch → disk cache → decode → RgbaImage
    │   └─ transition: Cut / Fade / SlideLeft / SlideRight
    └─ cache flush on exit
              │
              ▼
display_power module (Linux/Pi only)
    └─ vcgencmd display_power 0/1  →  HDMI on/off at schedule edges
              │
              ▼
Renderer (SDL2, SDL_VIDEODRIVER=kmsdrm on Linux)
    ├─ DRM probe (Linux only, startup once)
    │   └─ scans /dev/dri/card0..3 via drm crate → finds connected display
    └─ SDL2 canvas → present → /dev/dri/cardN → HDMI out
```

### SDL2 + DRM probe vs raw DRM

The Pi's VC4 GPU does not support DRM dumb buffers (`DRM_CAP_DUMB_BUFFER=0`).
Raw DRM would require GBM+EGL. SDL2's KMS backend already uses GBM+EGL internally
and is well-tested on Pi. The `drm` crate is used only to probe which card is active.

| Approach | Runtime deps | Works on VC4 | Complexity |
|---|---|---|---|
| Raw DRM dumb buffers | libdrm2 | No | High |
| Raw DRM + GBM + EGL | libdrm2 libgbm libEGL | Yes | Very high |
| **SDL2 + DRM probe (chosen)** | **libsdl2 libdrm2** | **Yes** | **Low** |

---

## Writing a new plugin

1. Create `plugins/my-source/Cargo.toml`:

```toml
[package]
name    = "picogallery-my-source"
version = "0.1.0"
edition = "2021"

[dependencies]
picogallery-core = { workspace = true }
anyhow           = { workspace = true }
async-trait      = { workspace = true }
log              = { workspace = true }
tokio            = { workspace = true }
```

2. Implement `PhotoPlugin` in `plugins/my-source/src/lib.rs`:

```rust
use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};
use async_trait::async_trait;

pub struct MyPlugin;

#[async_trait]
impl PhotoPlugin for MyPlugin {
    fn name(&self) -> &str { "my-source" }

    async fn init(&mut self, _cfg: &PluginConfig) -> anyhow::Result<()> { Ok(()) }
    async fn auth_status(&self) -> AuthStatus { AuthStatus::Authenticated }
    async fn authenticate(&mut self) -> anyhow::Result<AuthStatus> {
        Ok(AuthStatus::Authenticated)
    }
    async fn refresh_auth(&mut self) -> anyhow::Result<()> { Ok(()) }

    async fn list_photos(&self, limit: usize, offset: usize) -> anyhow::Result<Vec<PhotoMeta>> {
        Ok(vec![])
    }

    async fn get_photo_bytes(&self, meta: &PhotoMeta, dw: u32, dh: u32) -> anyhow::Result<Vec<u8>> {
        Ok(vec![])
    }
}
```

3. Add to root `Cargo.toml`:

```toml
# [workspace] members:
"plugins/my-source"

# [dependencies]:
picogallery-my-source = { path = "plugins/my-source", optional = true }

# [features]:
plugin-my-source = ["dep:picogallery-my-source"]
```

4. Register in `src/main.rs` `build_plugins()`:

```rust
#[cfg(feature = "plugin-my-source")]
if let Some(pcfg) = cfg.plugin_config("my-source") {
    plugins.push(Box::new(picogallery_my_source::MyPlugin::new(pcfg.clone())));
}
```

---

## Performance tuning for Pi Zero

```toml
[display]
transition    = "cut"   # no animation — saves CPU
transition_ms = 0
fps           = 10
fill_screen   = false   # letterbox cheaper than crop+scale

[cache]
prefetch_count = 2
max_mb         = 128
```

Set `gpu_mem=64` in `/boot/config.txt` (the installer does this).

For the WebDAV plugin on Pi Zero, set a low sync interval to avoid competing with the renderer:

```toml
[[plugins]]
name               = "webdav"
sync_interval_secs = 7200   # sync every 2 hours instead of 1
```

---

## CI/CD Pipeline

PicoGallery uses GitHub Actions to cross-compile for Raspberry Pi and publish pre-built binaries to GitHub Releases automatically.

### Pipeline file

`.github/workflows/release.yml`

### Triggers

| Event | Result |
|-------|--------|
| Push to `main` | Builds both targets; uploads as workflow artifacts (30-day retention) |
| Push a tag `v*` (e.g. `v0.2.0`) | Builds both targets; creates a GitHub Release with downloadable archives |
| Manual dispatch | Builds both targets; optionally creates a GitHub Release |

### To publish a new release

```bash
git tag v0.2.0
git push origin v0.2.0
```

The pipeline runs automatically. Within a few minutes, the release appears at:
`https://github.com/kethanva/pico-gallery/releases`

### Build matrix

| Target triple | Arch label | Devices |
|---------------|------------|---------|
| `aarch64-unknown-linux-gnu` | `linux-aarch64` | Pi Zero 2 W, Pi 3/4/5 (64-bit OS) |
| `armv7-unknown-linux-gnueabihf` | `linux-armv7` | Pi 2/3/4 (32-bit OS) |

### How the cross-compilation works

The pipeline runs on `ubuntu-24.04` and cross-compiles without QEMU:

1. Adds the target ARM architecture to apt (`dpkg --add-architecture arm64` / `armhf`)
2. Installs the GNU cross-toolchain (`gcc-aarch64-linux-gnu` or `gcc-arm-linux-gnueabihf`)
3. Installs cross-architecture sysroot packages: `libdrm-dev`, `libgbm-dev`, `libudev-dev`
4. SDL2 is built from source via the `bundled` cargo feature (cmake + cross-compiler) — no ARM SDL2 package required
5. Sets `CARGO_TARGET_*_LINKER`, `CC`, `PKG_CONFIG_PATH`, and `PKG_CONFIG_ALLOW_CROSS` for the build
6. Runs `cargo build --release --target <triple>`
7. The `profile.release` section in `Cargo.toml` already strips the binary (`strip = true`)

### Artifact contents

Each `.tar.gz` release archive contains:

```
picogallery-<version>-linux-<arch>/
├── picogallery          # stripped release binary
├── picogallery.service  # systemd unit file
├── config.example.toml  # annotated config template
├── LICENSE
└── README.md
```

Each archive has a matching `.sha256` checksum file. The installer verifies this automatically.

---

## Display without keyboard (GPIO button)

Use `triggerhappy` — reads `/dev/input/event*` directly, no X server needed.

```bash
sudo apt-get install -y triggerhappy
```

`/etc/triggerhappy/triggers.d/picogallery.conf`:
```
KEY_NEXT     1    kill -USR1 $(pidof picogallery)
KEY_PREVIOUS 1    kill -USR2 $(pidof picogallery)
```

> Do not use `xdotool` — it requires X11.

---

## License

MIT — see [LICENSE](LICENSE).
