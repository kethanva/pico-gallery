# PicoGallery

> Lightweight, plugin-based photo slideshow for Raspberry Pi — no desktop environment required.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.75+-orange)
![Platform](https://img.shields.io/badge/Platform-Raspberry%20Pi%20Zero%2F1%2F2%2F3%2F4-red)
[![Build & Release](https://github.com/kethanva/pico-gallery/actions/workflows/release.yml/badge.svg)](https://github.com/kethanva/pico-gallery/actions/workflows/release.yml)

Renders straight to the KMS/DRM framebuffer via SDL2. Runs on a Pi Zero W with ~8 MB RSS.

---

## Photo source plugins

| Plugin | Best for | Requires |
|---|---|---|
| `directory` ★ default | USB drive, local folder, NAS mount | Nothing extra |
| `webdav` | Nextcloud, Synology, ownCloud — upload from phone | Network |
| `photoprism` | Another Pi (4/5) running PhotoPrism — AI tagging, faces, albums | Network |
| `google-photos` | Google Drive folder | rclone |
| `amazon-photos` | Amazon Photos library | LWA developer app |
| `local` | Multiple root paths | Nothing extra |

---

## Install on Raspberry Pi

```bash
curl -sSL https://raw.githubusercontent.com/kethanva/pico-gallery/main/install.sh | bash
```

Pin a version: `PICOGALLERY_VERSION=v0.1.0 bash <(curl -sSL …/install.sh)`
Force source build: `PICOGALLERY_BUILD=1 bash <(curl -sSL …/install.sh)`

Installer detects arch, downloads the pre-built binary (or builds from source if no release), installs runtime deps (`libsdl2-2.0-0`, `libdrm2`, `ca-certificates`, `rclone`), adds you to `video`/`render`/`input` groups, writes a default config to `~/.config/picogallery/config.toml`, enables the systemd service, and sets `gpu_mem=64`.

After install:

```bash
nano ~/.config/picogallery/config.toml   # pick a plugin
sudo systemctl restart picogallery
sudo journalctl -u picogallery -f
sudo reboot                              # apply gpu_mem + group changes
```

### Uninstall

```bash
sudo systemctl disable --now picogallery
sudo rm /etc/systemd/system/picogallery.service /usr/local/bin/picogallery
sudo systemctl daemon-reload
rm -rf ~/.config/picogallery ~/Pictures/PicoGallery   # optional
```

### Supported architectures

| Archive | Devices |
|---|---|
| `*-linux-aarch64.tar.gz` | Pi Zero 2 W, Pi 3/4/5 (64-bit OS) |
| `*-linux-armv7.tar.gz` | Pi 2/3/4 (32-bit OS) |

---

## Plugin: `directory` (default)

Default points at `~/Pictures/PicoGallery`. Drop JPEG/PNG/WebP/GIF there; sub-folders become albums.

```toml
[[plugins]]
name      = "directory"
enabled   = true
path      = "~/Pictures/PicoGallery"
order     = "shuffle"          # shuffle | alphabetical | date_modified
recursive = true
# allowed_albums = ["Vacation 2024", "Family"]
# rescan_interval_secs = 3600
```

---

## Plugin: `webdav` (Nextcloud / Synology / ownCloud)

Syncs photos from any WebDAV server to a local cache; serves offline. Background re-sync keeps the frame fresh. Pure Rust — no davfs2, no rclone.

WebDAV URL examples:
- Nextcloud: `https://cloud.example.com/remote.php/dav/files/USERNAME`
- Synology DSM: `https://nas.local:5006/photo`
- ownCloud: `https://cloud.example.com/remote.php/webdav`

```toml
[[plugins]]
name        = "webdav"
enabled     = true
url         = "https://cloud.example.com/remote.php/dav/files/USERNAME"
username    = "alice"
password    = "your-app-password"     # use an app password, not your login
remote_path = "/Photos"
sync_dir    = "/tmp/picogallery-webdav"
sync_interval_secs = 3600             # 0 = startup only
# skip_tls_verify = true              # self-signed LAN cert
```

Upload from anywhere: Nextcloud mobile / web / desktop apps, Finder (`Go → Connect to Server`), Windows mapped network drive, or `rclone copy`.

---

## Plugin: `photoprism` (stream from a PhotoPrism server)

Thin REST client for a [PhotoPrism](https://www.photoprism.app) server — typically a Pi 4/5 (or any always-on host) running PhotoPrism in Docker, with the Pi Zero as the display "client". No local sync and no SD-card writes: the plugin opens one session, lists photos via `GET /api/v1/photos`, and streams the smallest pre-generated thumbnail that still fills the display. RAM-cheap enough for a Pi Zero 2 W.

```
┌─────────────────┐        LAN / HTTP         ┌──────────────────────┐
│  Pi Zero 2 W    │  ─── GET /api/v1/... ───▶  │  Pi 4/5 (or NAS/PC)  │
│  picogallery    │  ◀── JPEG thumbnails ───   │  PhotoPrism + Docker │
│  (this plugin)  │                            │  photo library       │
└─────────────────┘                            └──────────────────────┘
```

### What you need

| On the **server** (Pi 4/5, NAS, PC) | On the **client** (the Pi running picogallery) |
|---|---|
| Docker + `docker compose` | picogallery built with the `photoprism` feature (default build includes it) |
| PhotoPrism container reachable on the LAN (default port `2342`) | Network route to the server (same LAN, or VPN) |
| An admin user + password (or app password) | `url`, `username`, `password` in `[[plugins]]` |
| Photos imported and **indexed** (thumbnails generated) | — |

The plugin only reads pre-generated thumbnails, so the **photos must be indexed on the server first** — an un-indexed library returns zero photos.

### Step 1 — Stand up the PhotoPrism server

On the server machine (e.g. a Pi 4/5), create `~/photoprism/docker-compose.yml`. SQLite is fine for small/test libraries; for anything over a few thousand photos use the [official MariaDB compose](https://docs.photoprism.app/getting-started/raspberry-pi/) instead.

```yaml
# ~/photoprism/docker-compose.yml  (SQLite — simplest)
services:
  photoprism:
    image: photoprism/photoprism:latest
    container_name: photoprism
    restart: unless-stopped
    ports:
      - "2342:2342"
    security_opt:
      - seccomp:unconfined
      - apparmor:unconfined
    environment:
      PHOTOPRISM_ADMIN_USER:     "admin"
      PHOTOPRISM_ADMIN_PASSWORD: "CHANGE-ME"      # ≥ 8 chars
      PHOTOPRISM_AUTH_MODE:      "password"
      PHOTOPRISM_SITE_URL:       "http://photoprism.local:2342/"
      PHOTOPRISM_HTTP_HOST:      "0.0.0.0"        # listen on the LAN, not just localhost
      PHOTOPRISM_HTTP_PORT:      "2342"
      PHOTOPRISM_DATABASE_DRIVER: "sqlite"
      PHOTOPRISM_DISABLE_TLS:    "true"           # plain HTTP on a trusted LAN
    volumes:
      - "./originals:/photoprism/originals"       # drop your photos here
      - "./storage:/photoprism/storage"           # DB, cache, generated thumbnails
```

Bring it up and load photos:

```bash
cd ~/photoprism
mkdir -p originals storage
cp -r /path/to/your/photos/* originals/   # or import later via the web UI
docker compose up -d

# Watch it come up, then open the UI to import + index:
docker logs -f photoprism                 # wait for "http server started"
#   → browse http://photoprism.local:2342  (login: admin / CHANGE-ME)
#   → Library ▸ Index  (generates the thumbnails the plugin streams)
```

`PHOTOPRISM_HTTP_HOST=0.0.0.0` is the key line — without it PhotoPrism only listens on localhost and the Pi Zero can't reach it. Confirm reachability from the client Pi:

```bash
curl -fsS http://photoprism.local:2342/api/v1/status   # → {"status":"operational"}
```

If `photoprism.local` doesn't resolve, use the server's IP (`http://192.168.1.50:2342`) everywhere instead.

### Step 2 — Point picogallery at it (client config)

Edit `~/.config/picogallery/config.toml` on the Pi Zero. Disable other plugins, enable this one:

```toml
[[plugins]]
name     = "photoprism"
enabled  = true
url      = "http://photoprism.local:2342"   # base URL, NO trailing /api
username = "admin"
password = "CHANGE-ME"
# app_password = "abcd-efgh-ijkl-mnop"   # PhotoPrism v0.10+: Settings ▸ Account ▸ Apps
                                         #   and Devices — revocable per-device, preferred
                                         #   over the admin password on a wall display

# ── Filters (all optional, combined with AND) ──────────────────────────────
# album      = "january-2024"            # album UID or slug
# favorites  = true                      # only favourites
# quality    = 3                         # 1=low … 5=excellent (drops lower)
# country    = "fr"                      # ISO country code
# year       = 2024
# media_type = "image"                   # image | raw | live | animated | video
# query      = "label:beach keyword:sunset"  # raw PhotoPrism Q-language (appended)

# ── Ordering / paging ──────────────────────────────────────────────────────
# order    = "newest"                    # newest | oldest | added | name | random | similar
# per_page = 100                         # photos fetched per request (1–1000)

# ── Thumbnail size — saves Pi Zero RAM ─────────────────────────────────────
# Sizes (longest edge px): tile_500, fit_720, fit_1280, fit_1920, fit_2048,
#                          fit_2560, fit_3840, fit_4096, fit_7680
# max_thumb      = "fit_1920"            # cap requested size at 1920px → ~8 MB peak at 1080p
# allow_original = true                  # fall back to /dl/<hash> when no thumb is big enough

# ── Transport ──────────────────────────────────────────────────────────────
# skip_tls_verify      = false           # true only for self-signed HTTPS on the LAN
# request_timeout_secs = 30
```

Restart and watch the log:

```bash
sudo systemctl restart picogallery
sudo journalctl -u picogallery -f
#   look for:  PhotoPrism: logged in as admin (session abcd1234…)
```

### How auth + fetching works

- **Login:** `POST /api/v1/session` with the username/password (or app password). The returned session ID plus preview/download tokens are cached and reused; the plugin auto-re-logs in on an HTTP 401 and sends `DELETE /api/v1/session/{id}` on shutdown.
- **Listing:** `GET /api/v1/photos?count=&offset=&order=&merged=true&q=<filters>` — typed filters above are translated to PhotoPrism's `key:value` Q-syntax and merged with any raw `query`.
- **Image bytes:** thumbnail via `GET /api/v1/t/{hash}/{preview_token}/{size}`, or the original via `GET /api/v1/dl/{hash}?t={download_token}` when `allow_original = true` and no thumbnail is large enough. Video-only items are skipped.

### Pi Zero RAM profile

The plugin picks the smallest thumbnail whose longest edge ≥ the display, capped by `max_thumb`:

| Display | Thumbnail picked | Wire bytes | Peak decoded |
|---|---|---|---|
| 720×480   | `tile_500` | ~80 KB  | ~1 MB |
| 1280×720  | `fit_1280` | ~200 KB | ~3.6 MB |
| 1920×1080 | `fit_1920` | ~500 KB | ~8 MB |
| 3840×2160 | `fit_3840` | ~1.5 MB | ~32 MB |

### Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `login failed (HTTP 401)` | Wrong `username`/`password`, or `PHOTOPRISM_AUTH_MODE` isn't `password`. |
| Connection refused / timeout | Server bound to localhost — set `PHOTOPRISM_HTTP_HOST=0.0.0.0`; check firewall/port `2342`; verify with the `curl …/status` above. |
| Logs in but **no photos appear** | Library not indexed — run **Library ▸ Index** in the PhotoPrism UI; or filters too narrow (`album`/`year`/`query`). |
| `not a recognised image format` | Item is video-only or thumbnails aren't generated yet — re-index. |
| TLS / certificate errors | Self-signed LAN cert → set `skip_tls_verify = true` (LAN only). |

> **Want a throwaway server to test against first?** `dev/run-photoprism-local.sh` boots PhotoPrism on `http://localhost:2342` (admin / insecure), seeds and indexes a few photos, writes a matching config, and launches picogallery. See [Run locally](#run-locally-macos--linux-dev-box) below.

---

## Plugin: `google-photos` (Google Drive)

```toml
[[plugins]]
name            = "google-photos"
enabled         = true
sync_dir        = "/tmp/picogallery-gdrive"
drive_folder_id = ""        # paste a Drive folder ID, or "" for root
# max_transfer = "500"      # MB cap per sync run
```

First run opens a browser sign-in (`drive.readonly` scope, no Google Cloud project needed). Token saved to `~/.config/picogallery/rclone-gdrive.conf` and reused.

> **Google Photos Library API was removed on 2025-03-31.** This plugin uses **Google Drive** instead, which is unrestricted. If your photos are only in Google Photos and not in Drive, export with [Takeout](https://takeout.google.com) and use the `local` plugin.

---

## Plugin: `amazon-photos`

```toml
[[plugins]]
name          = "amazon-photos"
enabled       = true
client_id     = "YOUR_LWA_CLIENT_ID"
client_secret = "YOUR_LWA_CLIENT_SECRET"
```

Requires a Login with Amazon developer app — see developer.amazon.com.

---

## Plugin: `local` (multiple paths)

```toml
[[plugins]]
name      = "local"
enabled   = true
paths     = ["/mnt/usb/photos", "~/Pictures"]
recursive = true
```

---

## Display + scheduling

```toml
[display]
slide_duration_secs = 10
transition          = "fade"     # cut | fade | slide_left | slide_right
transition_ms       = 800        # 0 = instant
fill_screen         = false      # true = crop-to-fill; false = letterbox
fps                 = 15         # cap; lower = less CPU on Pi Zero
# width  = 1920                  # 0 = auto-detect
# height = 1080
order               = "shuffle"  # shuffle | chronological | newest_first
show_osd            = true       # metadata pill (album / date / filename) + nav arrows

# Optional HDMI on/off schedule (local time HH:MM, both required to activate)
# on_time  = "07:00"
# off_time = "22:00"             # overnight windows supported (e.g. 20:00 → 08:00)

# Memory safety — photos exceeding these are skipped with WARN, no crash
# max_image_mb   = 20            # raw file cap (0 = built-in 50 MB default)
# max_megapixels = 16            # decoded pixel cap (0 = unlimited)

[cache]
max_mb         = 256
prefetch_count = 3               # ≤3 on Pi Zero
# dir = "/tmp/picogallery-cache"
```

Schedule fires `vcgencmd display_power 0/1` on Pi to cut HDMI power. On non-Pi Linux/macOS the call is a no-op; the black frame is the only effect.

**Navigation:** `→`/`Space` next, `←` prev, `P` pause, `Q`/`Esc` quit. Left mouse click goes back, right click goes forward. When `show_osd = true`, ◄ and ► arrow pills are rendered on the left and right screen edges as a visual hint. Set `show_osd = false` to hide all overlays (metadata pill + arrows).

See `config.example.toml` for every key with inline comments.

---

## Hardware

| Device | Notes |
|---|---|
| Pi Zero W / 2W | Tested; `--jobs 1` when cross-compiling |
| Pi 2/3/4/5 | Full speed |

**Required:** display connected before boot (HDMI or DSI). **Not required:** keyboard, mouse, X, desktop.

System deps: `libsdl2-2.0-0`, `libdrm2`, `ca-certificates` (HTTPS), `rclone` (Google Drive only).

---

## Pi Zero memory tips

Photos are decoded → scaled → EXIF-rotated → displayed. Peak RAM ≈ `MP × 4 MB + ~8 MB display copy`.

| Source | Decoded RGBA | Peak |
|---|---|---|
| 12 MP | 48 MB | ~56 MB |
| 24 MP | 96 MB | ~104 MB |
| 48 MP | 192 MB | ~200 MB |

Pi Zero W has 512 MB shared with the GPU. With `gpu_mem=64` (installer default) ~448 MB is free. Recommended lean config:

```toml
[display]
transition     = "cut"
transition_ms  = 0
fps            = 10
show_osd       = false
max_image_mb   = 20
max_megapixels = 16

[cache]
prefetch_count = 2
max_mb         = 128
```

For PhotoPrism, set `max_thumb = "fit_1920"`. For WebDAV, raise `sync_interval_secs` to 7200 so syncs do not fight the renderer.

---

## Run locally (macOS / Linux dev box)

Runs on macOS and Linux without a Pi. SDL2 uses Cocoa/Metal on macOS; KMS/DRM is compiled out on non-Linux.

```bash
# macOS
brew install sdl2 pkg-config rclone cmake
# Linux
sudo apt-get install -y libsdl2-dev pkg-config cmake clang build-essential rclone

cargo build                                                          # all plugins
cargo build --no-default-features --features plugin-directory        # minimal
cargo test --workspace
```

### Option 1 — one-shot directory runner (`./run.sh`)

Builds, runs unit tests, generates 350 test photos under `/tmp/picogallery-e2e/`, writes a config pointing at them, and launches the binary. Flags: `--no-launch`, `--photos /path/to/your/library`.

```bash
./run.sh                       # test fixtures
./run.sh --photos ~/Pictures   # your own library
```

### Option 2 — local PhotoPrism stack (`dev/run-photoprism-local.sh`)

Boots a PhotoPrism container on `http://localhost:2342` (admin / insecure), seeds a few test JPEGs, runs plugin tests, writes a config that uses the `photoprism` plugin, and launches picogallery.

```bash
dev/run-photoprism-local.sh                       # bring up Docker + run
dev/run-photoprism-local.sh --no-launch           # stack + config + build only
dev/run-photoprism-local.sh --down                # stop the stack
dev/run-photoprism-local.sh --url http://pi.local:2342 --user admin --pass secret
                                                  # point at an existing server
```

Requires Docker + `docker compose` for the default mode. The compose file lives in `dev/photoprism/`; photo originals persist in `dev/photoprism/originals/`, PhotoPrism DB/cache in `dev/photoprism/storage/` (both gitignored).

### Option 3 — hand-rolled

```bash
mkdir -p /tmp/test-photos          # drop a few JPEGs here
cargo run -- --generate-config     # writes ~/.config/picogallery/config.toml
$EDITOR ~/.config/picogallery/config.toml
cargo run -- --config ~/.config/picogallery/config.toml --log-level debug
```

---

## Writing a new plugin

1. `plugins/my-source/Cargo.toml` — depend on `picogallery-core`, `anyhow`, `async-trait`, `tokio`.
2. Implement `PhotoPlugin` (see `core/src/lib.rs` for the trait).
3. Root `Cargo.toml`: add to `[workspace] members`, add optional dep, add `plugin-my-source` feature.
4. `src/main.rs::build_plugins()`: register behind `#[cfg(feature = "plugin-my-source")]`.

Existing plugins under `plugins/` are the reference implementations — `directory` is the simplest, `photoprism` shows REST + session auth, `webdav` shows background sync.

---

## Project layout

```
picogallery/
├── core/          picogallery-core: PhotoPlugin trait, PhotoMeta, PluginConfig
├── src/           binary: config, cache, renderer, slideshow, osd, exif_util,
│                  display_power, main (plugin registry)
└── plugins/
    ├── directory/    default; album support
    ├── local/        multi-path filesystem
    ├── webdav/       Nextcloud/Synology/ownCloud → local sync
    ├── photoprism/   PhotoPrism REST client (streaming, thumbnail-aware)
    ├── google-photos/  Google Drive via rclone
    └── amazon-photos/  Amazon Photos via LWA
```

Engine talks only to `dyn PhotoPlugin`. The Pi's VC4 GPU lacks DRM dumb-buffer support, so SDL2's KMS backend (GBM+EGL internally) is used; the `drm` crate only probes which `/dev/dri/cardN` is active.

---

## Releases

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which cross-compiles `aarch64-unknown-linux-gnu` and `armv7-unknown-linux-gnueabihf` on `ubuntu-24.04` and publishes `.tar.gz` + `.sha256` archives to GitHub Releases.

---

## License

MIT — see [LICENSE](LICENSE).
