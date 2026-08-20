# Project: PicoGallery Comprehensive Code Review

## Architecture & Codebase Layout
PicoGallery is a Rust workspace built for Raspberry Pi Zero 2 W (1-core CPU, 512MB RAM, systemd 384M cap) running KMS/DRM SDL2 display without X11/Wayland.
- `core/`: Common traits (`PhotoPlugin`), data models (`PhotoMeta`, `PluginConfig`), thumbnail and EXIF helpers.
- `src/`: Root crate containing main event loop, renderer, slideshow logic, disk cache, remote control HTTP server, gallery grid, and peripheral controllers:
  - `main.rs`, `renderer.rs`, `slideshow.rs`, `cache.rs`, `fetcher.rs`, `remote.rs`, `gallery.rs`, `gallery_controller.rs`, `fullscreen_controller.rs`, `mode.rs`, `config.rs`, `compose.rs`, `osd.rs`, `menu.rs`, `night.rs`, `wifi.rs`, `cec_remote.rs`, `display_power.rs`, `exif_util.rs`.
- `plugins/`:
  - `directory/`: Local directory photo source
  - `local/`: Local storage photo source
  - `usb/`: USB storage auto-mount photo source
  - `webdav/`: WebDAV remote photo source
  - `photoprism/`: PhotoPrism REST API source
  - `google-photos/`: Google Photos Library API OAuth source
  - `amazon-photos/`: Amazon Photos API source (opt-in feature)

## Code Review Scope & Streams
| Stream | Focus Area | Target Modules & Files | Status |
|---|---|---|---|
| Stream 1: Core & Rendering | Display loop, SDL2 KMS/DRM, composition, night filter, gallery grid, OSD, menus | `core/*`, `src/renderer.rs`, `src/compose.rs`, `src/night.rs`, `src/gallery.rs`, `src/osd.rs`, `src/menu.rs`, `src/mode.rs` | DONE |
| Stream 2: Concurrency & Runtime | Single-core Tokio safety, slideshow prefetching, disk LRU cache, raw TCP remote HTTP server, main loop | `src/slideshow.rs`, `src/fetcher.rs`, `src/cache.rs`, `src/remote.rs`, `src/main.rs`, `src/config.rs` | DONE |
| Stream 3: Cloud & Network Plugins | OAuth flows, basic auth, session tokens, network retry/timeouts, unwrap safety, cache key stability | `plugins/google-photos/*`, `plugins/amazon-photos/*`, `plugins/photoprism/*`, `plugins/webdav/*` | DONE |
| Stream 4: Local Plugins & Hardware | Local filesystem I/O, USB mounting, Wi-Fi configuration, CEC remote, display power, EXIF parsing, controllers | `plugins/directory/*`, `plugins/local/*`, `plugins/usb/*`, `src/wifi.rs`, `src/cec_remote.rs`, `src/display_power.rs`, `src/exif_util.rs`, `src/gallery_controller.rs`, `src/fullscreen_controller.rs` | DONE |

## Milestones
| # | Name | Scope | Dependencies | Status |
|---|------|-------|--------------|--------|
| M1 | Workspace Survey & Initialization | Set up audit framework, assign modules | None | DONE |
| M2 | Parallel Domain Audits | Stream 1-4 Deep-Dive Explorations | M1 | DONE |
| M3 | Cross-Domain Synthesis & Verification | Consolidate findings, deduplicate, verify severity & code fixes | M2 | DONE |
| M4 | Deliverable Generation | Compile `PicoGallery_Code_Review_Report.md` | M3 | DONE |

## Verified Findings Summary
- Total Findings: **43**
- Critical: **5** (S1-1, S2-01, S3-01, S3-02, S4-01)
- High: **12** (S1-2, S1-3, S2-02, S2-03, S3-03, S3-04, S3-05, S3-06, S4-02, S4-03, S4-04, S4-05)
- Medium: **13**
- Low: **13**
- Deliverable: `/Volumes/SSD/projects/PHOTOS_RELATED/pico-gallery/PicoGallery_Code_Review_Report.md`
