# PicoGallery Specification

## 1. Overview

PicoGallery is a photo-frame appliance for a **Raspberry Pi Zero 2 W running
headless** (no X11, no Wayland, no desktop). It renders directly to KMS/DRM via
SDL2 and pulls photos from pluggable sources. It is a *display client*, not a
photo server: it never owns, edits, or deletes the user's library.

## 2. Core stack

- **Language**: Rust 2021, MSRV **1.82**. Cargo workspace: root `picogallery` + `core` + `plugins/*`.
- **Runtime**: Tokio, `flavor = "current_thread"` only.
- **Graphics**: SDL2 (`use-pkgconfig`); `kmsdrm` driver on Linux, native backend on macOS for dev.
- **Imaging**: `image` (jpeg only), `fast_image_resize`, `zune-jpeg`, `jpeg-decoder` (no rayon), `qcms`, `stackblur-iter`.
- **Target hardware**: 1 ARM core, ~512 MB RAM, SD-card I/O. Service is capped at `MemoryMax=384M`, `TasksMax=128`.

## 3. Functional requirements

### 3.1 Photo sources (plugins)
Seven crates under `plugins/`, each implementing `PhotoPlugin` from `core`.
**Six are in `[features] default`** — `directory`, `local`, `usb`, `webdav`,
`photoprism`, `google-photos`. **`amazon-photos` is opt-in** and must be enabled
explicitly (`--features plugin-amazon-photos`); a stock build does not contain it.

Two independent gates: a plugin must be *compiled in* via its Cargo feature
**and** *enabled* at runtime via `[[plugins]]` in `config.toml`.

Plugins return **metadata only** (`PhotoMeta`); pixel bytes are fetched on
demand through `get_photo_bytes`.

### 3.2 Display
- Two modes: `Gallery` (scrollable thumbnail grid) and `Fullscreen` (slideshow / single photo).
- Next-N prefetch while the current slide is on screen, so transitions do not stall on SD-card or network I/O.
- Night mode: dim + warm tint, applied once per slide, never per frame.
- OSD, menu, Wi-Fi setup surface, HDMI-CEC TV-remote input, display power scheduling.

### 3.3 Remote control
Built-in HTTP server on a raw `TcpListener` (no framework, one parked accept
task). `GET /` serves a phone page; `POST /api/{next,prev,pause,favorite}`;
`GET /api/{status,health}`.

### 3.4 Caching
Disk LRU at `CacheConfig::resolved_dir()` — the configured `dir`, else
`dirs::cache_dir()/picogallery`, else `/tmp/picogallery`. Files are
`<sanitised_key>-<fnv1a_hash>.jpg`; index in `index.json`, rebuilt by scanning
the directory on startup. Cache key is `{plugin_name}/{id}` and is a
compatibility surface — changing it forces every device to re-download.

## 4. Non-functional requirements

- **Single core is the budget.** No multi-threaded Tokio scheduler, no rayon. Anything that competes with the display loop is a regression.
- **Binary size matters, throughput matters more.** `opt-level = 2` + `lto = true`, deliberately not `"z"`.
- **Unattended operation.** The device has no keyboard and no user watching. A panic is a bricked frame; only `Restart=on-failure` recovers it.
- **Least privilege.** The service runs as the configured service user
  (normally `pi`) with `Group=video`, `ProtectSystem=full`,
  `ProtectKernelTunables/Modules/ControlGroups`, `LockPersonality`,
  `RestrictSUIDSGID`, `UMask=0077`, and a narrow `ReadWritePaths` policy where
  the installed unit provides it. Wi-Fi and USB mounting are delegated to
  host policy (NetworkManager/udisks) rather than granting privileges.

## 5. Failure modes & required behavior

| Condition | Required behavior |
|---|---|
| One plugin fails to initialize | Log at `warn`/`error`, continue with the rest. **Not fatal.** |
| *All* enabled plugins fail | Fatal startup error pointing at `journalctl -u picogallery`. |
| `/dev/dri/card*` probe fails | Log the cause; do not panic. |
| Network source unreachable mid-run | Keep serving cached photos; retry, do not exit. |
| Corrupt / undecodable image | Skip that photo, log, advance. |
| Cache directory unwritable | Degrade to no-cache operation rather than failing startup. |
| Config file absent | Fall back to defaults; `--generate-config` writes a template. |
| Sign-in not completed within `auth.pending_timeout_secs` | Disable that source, log, continue with the rest. Not fatal unless every enabled source fails. |

## 6. Security requirements

- Credentials are supplied by `config.toml` or `PICOGALLERY_{PLUGIN}_{KEY}` env overrides. `apply_env_secret` treats a set-but-empty var as an error, never as "unset".
- `config.rs` strips inline secret values before writing `config.toml` back to disk.
- `PhotoMeta.extra` is serialized into the cache and must carry only stable, non-secret identifiers. Session credentials and bearer tokens stay in plugin session state.
- External remote-control binds require authentication; do not expose the remote on an untrusted network without it.
- Never commit `config.toml`, `deploy.local.env`, `*credentials*`, `client_secret*`, `*.pem`, `*.key`.

## 7. Out of scope

- Acting as a photo server, or any pixel write/delete/edit path against the user's library.
  **Carve-out:** `POST /api/favorite` may toggle the source's own favourite *flag* on plugins that advertise `PluginCapabilities::favorite_toggle` (PhotoPrism). That is a metadata write, touches no pixels, and deletes nothing.
- Desktop-environment or X11/Wayland dependencies.
- Multi-threaded work-stealing execution.
- Video playback.
