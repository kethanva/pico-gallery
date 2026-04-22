# PicoGallery

> Lightweight, plugin-based photo slideshow for Raspberry Pi — no desktop environment required.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.75+-orange)
![Platform](https://img.shields.io/badge/Platform-Raspberry%20Pi%20Zero%2F1%2F2%2F3%2F4-red)
[![Build & Release](https://github.com/kethanva/pico-gallery/actions/workflows/release.yml/badge.svg)](https://github.com/kethanva/pico-gallery/actions/workflows/release.yml)

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

## Project structure

```
picogallery/
├── Cargo.toml                    # workspace root + main crate (lib + bin)
├── Cargo.lock
├── install.sh                    # one-shot Pi installer
├── core/                         # picogallery-core: shared plugin trait
│   ├── Cargo.toml
│   └── src/lib.rs                # PhotoPlugin trait, PhotoMeta, PluginConfig
├── src/
│   ├── lib.rs                    # re-exports core + all modules
│   ├── main.rs                   # binary entry point + plugin registry
│   ├── config.rs                 # TOML config structs
│   ├── cache.rs                  # LRU disk image cache
│   ├── renderer.rs               # SDL2 / KMS-DRM renderer
│   └── slideshow.rs              # async slideshow engine
└── plugins/
    ├── google-photos/            # Google Drive plugin (drive.readonly via rclone)
    ├── amazon-photos/            # Amazon Photos via LWA
    ├── directory/                # Directory plugin (recommended for local files)
    └── local/                    # Local filesystem scanner
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

## Features

- **No X11 / no desktop** — renders directly to the KMS/DRM framebuffer via SDL2.
- **Plugin architecture** — Google Drive, Amazon Photos, local directory/filesystem; add your own with one Rust trait.
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
rclone            Google Drive sync (no API key — uses rclone's verified OAuth)
ca-certificates   HTTPS root certs
```

---

## Quick start

### 1. Install rclone

```bash
# macOS
brew install rclone

# Raspberry Pi / Debian
sudo apt install rclone
```

### 2. Configure

Create `~/.config/picogallery/config.toml`:

```toml
[display]
slide_duration_secs = 10
transition          = "fade"    # "cut" | "fade" | "slide_left" | "slide_right"
transition_ms       = 800
fill_screen         = false
fps                 = 15

[cache]
max_mb         = 256
prefetch_count = 3

# Google Drive — no API key or Cloud project needed
[[plugins]]
name             = "google-photos"
enabled          = true
sync_dir         = "/tmp/picogallery-gdrive"
drive_folder_id  = ""            # leave blank for Drive root, or paste a folder ID
```

### 3. Run

```bash
picogallery --config ~/.config/picogallery/config.toml
```

**First run only:** a browser window opens for a one-time Google sign-in
(rclone's own verified OAuth — nothing to set up in Google Cloud).
Approve access, and the app stores the token at
`~/.config/picogallery/rclone-gdrive.conf`.

Every subsequent run starts immediately with no sign-in prompt.
rclone syncs images in the background while the slideshow runs.

### 4. Enable on boot (Pi)

```bash
sudo systemctl enable --now picogallery
```

---

## Configuration reference

```toml
[display]
slide_duration_secs = 10      # seconds per photo
transition          = "fade"  # "cut" | "fade" | "slide_left" | "slide_right"
transition_ms       = 800     # transition duration in ms
fill_screen         = false   # true=crop-to-fill, false=letterbox
fps                 = 15      # max FPS (lower = less CPU on Pi Zero)
# width  = 1920               # force resolution (0 = auto-detect)
# height = 1080

[cache]
max_mb         = 256   # disk cache ceiling
prefetch_count = 3     # photos to prefetch ahead

# ── Google Drive (rclone backend) ─────────────────────────────────────────────
[[plugins]]
name             = "google-photos"
enabled          = true
sync_dir         = "/tmp/picogallery-gdrive"   # local cache directory
drive_folder_id  = ""                          # specific Drive folder ID, or "" for root
# max_transfer   = "500"                       # MB cap per sync run

# ── Amazon Photos ─────────────────────────────────────────────────────────────
# [[plugins]]
# name          = "amazon-photos"
# enabled       = true
# client_id     = "YOUR_LWA_CLIENT_ID"
# client_secret = "YOUR_LWA_CLIENT_SECRET"

# ── Directory Plugin (Recommended for local photos) ───────────────────────────
# [[plugins]]
# name      = "directory"
# enabled   = true
# path      = "/home/pi/Photos"
# order     = "shuffle"         # "shuffle" | "alphabetical" | "date_modified"
# recursive = true

# ── Local filesystem (Legacy Multi-path) ──────────────────────────────────────
# [[plugins]]
# name    = "local"
# enabled = false
# paths   = ["/mnt/nas/photos", "/home/pi/Pictures"]
```

---

## Google Photos — API status (March 2025)

> **Google removed `photoslibrary.readonly` on March 31, 2025.**
> All read access to existing photo libraries via the Google Photos API is permanently blocked.
> This affects the Photos Library API, rclone's Google Photos backend, and all third-party tools.

### Recommended: Use Google Drive

PicoGallery's `google-photos` plugin now uses **Google Drive** (`drive.readonly`) as its backend,
which is fully accessible and unrestricted.

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
4. Copy to your Pi (USB drive, NAS, or `scp`) and configure:

```toml
[[plugins]]
name    = "local"
enabled = true
paths   = ["/mnt/photos/Google Photos"]   # path to your Takeout extract
```

5. Re-export every few months to pick up new photos

### API status table

| Approach | Status (2026) |
|---|---|
| Google Photos Library API | Blocked for existing libraries since March 2025 |
| rclone Google Photos backend | Blocked (uses same underlying API) |
| Google Picker API | Manual per-session selection — not suitable for slideshows |
| **Google Drive plugin (this app)** | **Works — `drive.readonly`, no restrictions** |
| **Google Takeout + local plugin** | **Works — recommended if photos not in Drive** |
| Self-hosted Immich / Nextcloud | Works — full open API control |

---

## Running locally for development

The app runs on macOS and Linux without a Pi. SDL2 uses its native backend
(Cocoa/Metal on macOS). KMS/DRM is compiled out on non-Linux platforms.

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
# Full build (Google Drive + Amazon Photos + directory + local)
cargo build

# Directory-only build (no rclone needed)
cargo build --no-default-features --features plugin-directory
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

### Run with Google Drive (macOS)

```bash
SDL_LIB=$(find target/debug/build -name "libSDL2-2.0.0.dylib" | head -1 | xargs dirname)
DYLD_LIBRARY_PATH="$SDL_LIB" target/debug/picogallery --config ~/.config/picogallery/config.toml
```

First run opens a browser for a one-time Google sign-in. Token is saved automatically at
`~/.config/picogallery/rclone-gdrive.conf`. Every subsequent run starts immediately.

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

---

## Architecture overview

```
config.toml
    │
    ▼
Plugin registry ──┬── GoogleDrivePlugin (rclone drive.readonly → local disk)
                  ├── AmazonPhotosPlugin (LWA OAuth → Amazon API)
                  ├── DirectoryPlugin (local filesystem scanning with sorting)
                  └── LocalPlugin (legacy filesystem scan)
                        │  dyn PhotoPlugin
                        ▼
                  Slideshow engine
                  ├─ build_queue(): list photos from plugins, shuffle
                  ├─ prefetch loop: fetch → disk cache → decode
                  └─ display loop: transition → show → event poll
                        │
                        ▼
                  Renderer (SDL2, SDL_VIDEODRIVER=kmsdrm on Linux)
                  │
                  ├─ DRM probe (Linux only, startup once)
                  │   └─ scans /dev/dri/card0..3 via drm crate
                  │   └─ finds connected display, native resolution
                  │   └─ sets SDL_VIDEO_KMSDRM_DEVICE=correct cardN
                  │
                  └─ SDL2 canvas → present → /dev/dri/cardN → HDMI
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
