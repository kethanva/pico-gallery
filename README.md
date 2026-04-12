# 🖼 PicoGallery

> Lightweight, plugin-based photo slideshow for Raspberry Pi — no desktop environment required.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.75+-orange)
![Platform](https://img.shields.io/badge/Platform-Raspberry%20Pi%20Zero%2F1%2F2%2F3%2F4-red)

---

## Project structure

```
picogallery/
├── Cargo.toml                    # workspace root + main crate (lib + bin)
├── Cargo.lock                    # locked dependency versions (reproducible builds)
├── install.sh                    # one-shot Pi installer
├── core/                         # picogallery-core: shared plugin trait (no cycle)
│   ├── Cargo.toml
│   └── src/lib.rs                # PhotoPlugin trait, PhotoMeta, PluginConfig
├── src/
│   ├── lib.rs                    # re-exports core + all modules
│   ├── main.rs                   # binary entry point + plugin registry
│   ├── config.rs                 # TOML config structs (DisplayConfig, Transition, …)
│   ├── cache.rs                  # LRU disk image cache
│   ├── renderer.rs               # SDL2 / KMS-DRM renderer (Linux) or native (macOS)
│   └── slideshow.rs              # async slideshow engine + background prefetch
└── plugins/
    ├── google-photos/            # Google Photos via OAuth2 device flow
    │   ├── Cargo.toml
    │   └── src/lib.rs
    ├── amazon-photos/            # Amazon Photos via LWA device flow
    │   ├── Cargo.toml
    │   └── src/lib.rs
    └── local/                    # Local filesystem scanner
        ├── Cargo.toml
        └── src/lib.rs
```

---

## Why Rust? Why not Python?

| | **Rust (chosen)** | Python |
|---|---|---|
| RSS on Pi Zero | ~8 MB | ~60–120 MB |
| Binary size | ~4 MB stripped | N/A (interpreter) |
| CPU during decode | ~40 % one core | ~90 % one core |
| Startup time | < 0.5 s | 2–5 s |
| Packages installed | `libsdl2-2.0-0` only | python3, pip, 15+ wheels |
| GC pauses during fade | None | Yes (Python GC) |

On a Pi Zero with 512 MB RAM, every megabyte matters. Rust wins clearly.

---

## Features

- **No X11, no desktop environment** — renders directly to the KMS/DRM framebuffer via SDL2.
- **Plugin architecture** — Google Photos, Amazon Photos, and local filesystem today; add your own by implementing a single Rust trait.
- **OAuth2 device flow** — headless-friendly authentication: a URL and code are shown on screen; the user approves on any browser.
- **Disk cache with LRU eviction** — photos are cached to SD card so they survive reboots and brief WiFi outages.
- **Background prefetch** — next N photos are fetched while the current one displays, so transitions are instant.
- **Cross-fade / slide / cut transitions** — configurable per config.toml. `cut` is fastest on Pi Zero.
- **Keyboard control** — `→` / `Space` next, `←` prev, `P` pause, `Q` / `Esc` quit.

---

## Hardware requirements

| Device | Notes |
|---|---|
| Raspberry Pi Zero W / 2W | Tested; use `--jobs 1` when building |
| Raspberry Pi 1 Model B+ | Should work; same ARM11 core as Zero |
| Raspberry Pi 2 / 3 / 4 | Full speed |

**Required**: a display connected before boot (HDMI or official DSI touchscreen).

**Not required**: keyboard, mouse, X server, desktop environment, display manager.

---

## System dependencies

Runtime packages (the only ones installed permanently):

```
libsdl2-2.0-0     ~1.5 MB   SDL2 with KMS/DRM backend
libdrm2           ~200 KB   DRM display probing (finds correct /dev/dri/cardN)
ca-certificates   ~200 KB   HTTPS root certificates (already present on Pi OS)
```

Build-time only (safe to `apt purge` after compiling):
```
libsdl2-dev  libdrm-dev  clang  pkg-config  build-essential
```

---

## Quick start

### 1. Install

```bash
# On your Pi (SSH in or directly):
curl -sSL https://raw.githubusercontent.com/yourusername/picogallery/main/install.sh | bash
```

The installer will:
- Install `libsdl2-dev`, `clang`, `pkg-config`
- Install Rust via rustup (if not present)
- Compile picogallery with release optimisations
- Install to `/usr/local/bin/picogallery`
- Add your user to the `video` and `render` groups
- Write a systemd service

### 2. Configure Google Photos credentials

You need a **TV and Limited Input Devices** OAuth 2.0 client. This type
supports the device flow, which is headless-compatible.

1. Go to [Google Cloud Console](https://console.cloud.google.com/)
2. Create a project (or use an existing one)
3. Enable **Photos Library API**
4. Go to **APIs & Services → Credentials → Create Credentials → OAuth client ID**
5. Choose **TV and Limited Input Devices**
6. Copy the `client_id` and `client_secret`

Edit your config:

```bash
nano ~/.config/picogallery/config.toml
```

```toml
[[plugins]]
name          = "google-photos"
enabled       = true
client_id     = "1234567890-abc.apps.googleusercontent.com"
client_secret = "GOCSPX-yourSecret"
```

### 3. First run (auth)

```bash
# From a terminal (SSH works fine):
SDL_VIDEODRIVER=kmsdrm picogallery
```

You will see something like:

```
=== Google Photos ===
Open this URL on any device:

  https://www.google.com/device

Enter code: ABCD-EFGH

(expires in 1800 seconds)
```

Open the URL on your phone or computer, enter the code, and approve.
The Pi will detect the approval and start the slideshow automatically.
The token is saved to `~/.config/picogallery/` so you only do this once.

### 4. Enable on boot

```bash
sudo systemctl enable --now picogallery
```

---

## Configuration reference

```toml
[display]
slide_duration_secs = 10      # seconds per photo
transition          = "fade"  # "cut" | "fade" | "slide_left" | "slide_right"
transition_ms       = 800     # transition length in milliseconds
fill_screen         = false   # true=crop-to-fill, false=letterbox
fps                 = 15      # max FPS (lower = less CPU; 15 is good for Pi Zero)

[cache]
max_mb         = 256   # disk cache ceiling
prefetch_count = 3     # photos to prefetch ahead

# Google Photos
[[plugins]]
name          = "google-photos"
enabled       = true
client_id     = "..."
client_secret = "..."
# album_id = "ABcd123..."  # restrict to one album

# Local files
[[plugins]]
name    = "local"
enabled = false
paths   = ["/mnt/nas/photos", "/home/pi/Pictures"]
```

---

## Writing a new plugin

1. Create a new crate under `plugins/`:

```
plugins/my-source/
├── Cargo.toml
└── src/lib.rs
```

`plugins/my-source/Cargo.toml`:
```toml
[package]
name    = "picogallery-my-source"
version = "0.1.0"
edition = "2021"

[dependencies]
picogallery = { path = "../.." }
anyhow      = { workspace = true }
async-trait = { workspace = true }
log         = { workspace = true }
tokio       = { workspace = true }
# add reqwest, serde, etc. as needed
```

2. Implement the `PhotoPlugin` trait in `plugins/my-source/src/lib.rs`:

```rust
use picogallery::plugin::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};
use async_trait::async_trait;

pub struct MyPlugin { /* fields */ }

#[async_trait]
impl PhotoPlugin for MyPlugin {
    fn name(&self) -> &str { "my-source" }

    async fn init(&mut self, config: &PluginConfig) -> anyhow::Result<()> { Ok(()) }
    async fn auth_status(&self) -> AuthStatus { AuthStatus::Authenticated }
    async fn authenticate(&mut self) -> anyhow::Result<AuthStatus> { Ok(AuthStatus::Authenticated) }

    async fn list_photos(&self, limit: usize, offset: usize) -> anyhow::Result<Vec<PhotoMeta>> {
        // Return metadata — no pixel data here.
        Ok(vec![])
    }

    async fn get_photo_bytes(&self, meta: &PhotoMeta, dw: u32, dh: u32) -> anyhow::Result<Vec<u8>> {
        // Return raw JPEG/PNG bytes.
        Ok(vec![])
    }
}
```

3. Register the crate in the root `Cargo.toml` (three places):

```toml
# Under [workspace] members:
members = [".", "plugins/my-source", ...]

# Under [dependencies]:
picogallery-my-source = { path = "plugins/my-source", optional = true }

# Under [features]:
plugin-my-source = ["dep:picogallery-my-source"]
```

4. Add to `build_plugins()` in `src/main.rs`:

```rust
#[cfg(feature = "plugin-my-source")]
{
    if let Some(pcfg) = cfg.plugin_config("my-source") {
        plugins.push(Box::new(picogallery_my_source::MyPlugin::new(pcfg.clone())));
    }
}
```

5. Add a `[[plugins]]` entry to `~/.config/picogallery/config.toml` and build with `--features plugin-my-source`.

---

## Running locally for development and testing

The app runs on **macOS and Linux** without a Pi. SDL2 uses its native backend
(Cocoa/Metal on macOS, X11/KMS on Linux). The KMS/DRM probe and `kmsdrm` video
driver are automatically disabled on non-Linux platforms.

### Prerequisites

**macOS**
```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"

# cmake is required to compile the bundled SDL2
# Download cmake universal binary from https://cmake.org/download/
# and add to PATH, or install via a package manager
```

**Linux (Ubuntu/Debian)**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
sudo apt-get install -y libsdl2-dev pkg-config cmake clang build-essential
```

### Build for local testing (local plugin only — no credentials needed)

```bash
cargo build --no-default-features --features plugin-local
```

### Create test photos

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

### Write a test config

```bash
mkdir -p ~/.config/picogallery
cat > ~/.config/picogallery/config.toml << 'EOF'
[display]
slide_duration_secs = 4
transition          = "fade"
transition_ms       = 600
fill_screen         = false
fps                 = 30

[cache]
max_mb         = 64
prefetch_count = 2

[[plugins]]
name    = "local"
enabled = true
paths   = ["/tmp/picogallery-test-photos"]
EOF
```

### Run

```bash
# macOS — SDL2 compiled from source (bundled), find its dylib first:
SDL_LIB=$(find target/debug/build -name "libSDL2-2.0.0.dylib" | head -1 | xargs dirname)
DYLD_LIBRARY_PATH="$SDL_LIB" RUST_LOG=info ./target/debug/picogallery

# Linux
RUST_LOG=info ./target/debug/picogallery

# Or with explicit config path:
RUST_LOG=info ./target/debug/picogallery --config ~/.config/picogallery/config.toml
```

Keys while running: `→` / `Space` next · `←` prev · `P` pause · `Q` / `Esc` quit

### Measured resource usage (macOS Apple Silicon, release build)

| Metric | Debug build | Release build |
|---|---|---|
| Binary size | 18 MB | **1.5 MB** |
| RSS at idle (photo on screen) | ~135 MB | **~1.6 MB** |
| RSS during transition | ~140 MB | **~2–3 MB** |
| CPU at idle | ~0.1% | **~0%** |
| CPU during fade transition | 54–101% (unoptimised) | ~10–15% (estimate) |

Build release for accurate numbers:
```bash
cargo build --release --no-default-features --features plugin-local
```

> **Note:** the debug build is large because it includes debug symbols, bounds checks,
> and unoptimised allocations. Always use `--release` for Pi deployment and benchmarking.

---

## Performance tuning for Pi Zero

```toml
[display]
transition  = "cut"   # no fade — saves CPU
fps         = 10      # reduce frame rate
fill_screen = false   # letterbox is cheaper than crop+scale
transition_ms = 0

[cache]
prefetch_count = 2    # fewer concurrent fetches
max_mb         = 128  # smaller if SD card is tight
```

Set `gpu_mem=64` in `/boot/config.txt` (the installer does this automatically).

---

## Display without keyboard

A GPIO button can send keyboard events to picogallery via `triggerhappy`.
`triggerhappy` reads `/dev/input/event*` directly — no X server needed.

Install:
```bash
sudo apt-get install -y --no-install-recommends triggerhappy
```

Wire a button to a GPIO pin and map it in `/etc/triggerhappy/triggers.d/picogallery.conf`:
```
# KEY_NEXT / KEY_PREVIOUS are sent by a GPIO button driver (e.g. gpio-keys overlay)
KEY_NEXT     1    picogallery-ctl next
KEY_PREVIOUS 1    picogallery-ctl prev
```

Alternatively, send `SIGUSR1`/`SIGUSR2` directly to advance slides:
```bash
kill -USR1 $(pidof picogallery)   # next photo
```

> **Do not use `xdotool`** — it requires an X server which this project intentionally avoids.

---

## Architecture overview

```
config.toml
    │
    ▼
Plugin registry ──┬── GooglePhotosPlugin (OAuth2 device flow → Photos API)
                  ├── AmazonPhotosPlugin (LWA device flow → Drive API)
                  └── LocalPlugin (filesystem scan)
                        │  dyn PhotoPlugin trait
                        ▼
                  Slideshow engine
                  ├─ build_queue(): page through plugins, shuffle
                  ├─ prefetch loop: async fetch → disk cache → decode
                  └─ display loop: transition → show → event poll
                        │
                        ▼
                  Renderer (SDL2, SDL_VIDEODRIVER=kmsdrm)
                  │
                  ├─ DRM probe (startup, once)
                  │   └─ scans /dev/dri/card0..3 via drm crate
                  │   └─ finds connected HDMI/DSI connector
                  │   └─ reads native resolution from preferred mode
                  │   └─ sets SDL_VIDEO_KMSDRM_DEVICE → correct cardN
                  │
                  └─ SDL2 KMS/DRM renderer
                        │
                        ▼
                  /dev/dri/cardN  (VC4 KMS/DRM — no X11)
                        │
                        ▼
                  HDMI / DSI display
```

### Why SDL2 + DRM probe rather than raw DRM?

The Raspberry Pi's VC4/V3D GPU driver does **not** support DRM dumb buffers
(`DRM_CAP_DUMB_BUFFER = 0` on vc4).  Going fully raw would require GBM buffer
management + EGL — adding `libgbm` and `libEGL` as runtime dependencies and
significant complexity for no user-visible benefit.

SDL2's KMS/DRM backend already uses GBM+EGL internally and is well-tested on Pi.
The `drm` crate is used only for the lightweight startup probe — no buffers are
allocated, no DRM master is claimed.

| Approach | Runtime libs | Works on VC4 | Complexity |
|---|---|---|---|
| Raw DRM dumb buffers | libdrm2 | ✗ (no dumb buffer cap) | High |
| Raw DRM + GBM + EGL | libdrm2 libgbm libEGL | ✓ | Very high |
| **SDL2 + DRM probe (chosen)** | **libsdl2 libdrm2** | **✓** | **Low** |

---

## License

MIT — see [LICENSE](LICENSE).
