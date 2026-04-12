# PicoGallery

> Lightweight, plugin-based photo slideshow for Raspberry Pi — no desktop environment required.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.75+-orange)
![Platform](https://img.shields.io/badge/Platform-Raspberry%20Pi%20Zero%2F1%2F2%2F3%2F4-red)

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
    ├── google-photos/            # Google Photos via rclone (no API key needed)
    ├── amazon-photos/            # Amazon Photos via LWA
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
- **Plugin architecture** — Google Photos, Amazon Photos, local filesystem; add your own with one Rust trait.
- **Google Photos via rclone** — no Google Cloud project or API key needed; rclone's own verified OAuth app handles auth.
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
rclone            Google Photos sync (no API key — uses rclone's verified OAuth)
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

# Google Photos — no API key or Cloud project needed
[[plugins]]
name     = "google-photos"
enabled  = true
sync_dir = "/tmp/picogallery-gphotos"   # local photo cache
# album  = "Holiday 2024"               # optional: one album only
```

### 3. Run

```bash
picogallery --config ~/.config/picogallery/config.toml
```

**First run only:** a browser window opens for a one-time Google sign-in
(rclone's own verified OAuth — nothing to set up in Google Cloud).
Approve access, and the app stores the token at
`~/.config/picogallery/rclone-gphotos.conf`.

Every subsequent run starts immediately with no sign-in prompt.
rclone syncs new photos in the background while the slideshow runs.

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

# ── Google Photos (rclone backend) ────────────────────────────────────────────
[[plugins]]
name          = "google-photos"
enabled       = true
sync_dir      = "/tmp/picogallery-gphotos"   # local cache directory
# album       = "Favourites"                 # sync one album only
# max_transfer = "500"                       # MB cap per sync run

# ── Local filesystem ──────────────────────────────────────────────────────────
# [[plugins]]
# name    = "local"
# enabled = true
# paths   = ["/mnt/nas/photos", "/home/pi/Pictures"]
```

---

## How Google Photos auth works

No Google Cloud project, no API key, no app verification process.

```
picogallery starts
       │
       ▼
token saved?  ──yes──▶  sync photos in background  ──▶  slideshow runs
       │
      no
       │
       ▼
rclone authorize "google photos"
  (uses rclone's own verified OAuth app)
       │
       ├── macOS: browser opens automatically
       └── Pi:    URL printed → open on phone/laptop
       │
       ▼
user approves in browser
       │
       ▼
token saved to ~/.config/picogallery/rclone-gphotos.conf
       │
       ▼
rclone syncs photos to sync_dir  ──▶  slideshow starts
```

The token is refreshed automatically by rclone on every sync.
Re-authentication is never required unless you revoke access.

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
# Full build (Google Photos + local)
cargo build

# Local-only build (no rclone needed)
cargo build --no-default-features --features plugin-local
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
name    = "local"
enabled = true
paths   = ["/tmp/picogallery-test-photos"]
EOF
```

### Run (macOS)

```bash
SDL_LIB=$(find target/debug/build -name "libSDL2-2.0.0.dylib" | head -1 | xargs dirname)
DYLD_LIBRARY_PATH="$SDL_LIB" target/debug/picogallery --config ~/.config/picogallery/config.toml
```

### Run with Google Photos (macOS)

```bash
SDL_LIB=$(find target/debug/build -name "libSDL2-2.0.0.dylib" | head -1 | xargs dirname)
DYLD_LIBRARY_PATH="$SDL_LIB" target/debug/picogallery --config ~/.config/picogallery/config.toml
```

First run opens a browser. After approving, the slideshow starts automatically.

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
Plugin registry ──┬── GooglePhotosPlugin (rclone sync → local disk)
                  ├── AmazonPhotosPlugin (LWA OAuth → Amazon API)
                  └── LocalPlugin (filesystem scan)
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
