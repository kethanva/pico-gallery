# Display Rendering: Options Analysis

> Research notes on the rendering stack for PicoGallery.
> Covers SDL2, direct DRM/KMS, and the hybrid approach currently implemented.

---

## Background — what is DRM/KMS?

**DRM (Direct Rendering Manager)** and **KMS (Kernel Mode Setting)** are Linux
kernel subsystems that manage the display pipeline without a user-space display
server (no X11, no Wayland compositor).

```
Application
    │
    ▼
/dev/dri/cardN          ← DRM device node
    │
    ▼
KMS (Kernel Mode Setting)
├─ Connector  — the physical output (HDMI, DSI, VGA…)
├─ Encoder    — converts signal for the connector
├─ CRTC       — scans a framebuffer out to the encoder
└─ Framebuffer — pixel buffer shown on screen
    │
    ▼
Display (HDMI monitor / DSI panel)
```

Key point: **no X server is needed**. The kernel talks directly to the GPU/display
hardware. This is why KMS/DRM is ideal for a headless Pi running only a slideshow.

---

## Rendering options evaluated

### Option A — Pure SDL2 with KMS/DRM backend (baseline)

SDL2 can use KMS/DRM as its video driver instead of X11:

```bash
SDL_VIDEODRIVER=kmsdrm ./picogallery
```

Internally SDL2 allocates GBM surfaces, sets up EGL, and page-flips via DRM —
all hidden behind the SDL2 API.

**Pros**
- Simple API (textures, canvas, event loop all built in)
- Well-tested on Pi hardware
- Works on VC4 (Pi's GPU driver)

**Cons**
- libsdl2 is ~1.5 MB on disk, ~4–8 MB RSS at runtime
- SDL2 event loop polls even when idle
- Each frame: RGBA → SDL_Texture → SDL_Renderer → EGL → GBM → DRM flip (2–3 extra copies)
- `static-link` feature bakes SDL2 into the binary (+~2 MB)

---

### Option B — Direct DRM with dumb buffers (investigated, rejected)

The lowest-level approach: allocate a "dumb buffer" (a simple CPU-writable
framebuffer) via the DRM ioctl, write pixels directly, and set the CRTC to
display it.

```
App → create_dumb_buffer → mmap → write pixels → set_crtc / page_flip → display
```

**Why it fails on Raspberry Pi**

The Pi's VC4/V3D GPU driver does **not** support dumb buffers:

```
DRM_CAP_DUMB_BUFFER = 0   (vc4 kernel driver)
```

This is a hard kernel-level limitation. Any Rust code using `drm::create_dumb_buffer`
will get an error on every current Pi model.

Rust crate that would be used: [`drm`](https://crates.io/crates/drm) v0.15 (smithay/drm-rs)

**Verdict: not viable on Pi.**

---

### Option C — Direct DRM with GBM buffers (viable but complex)

GBM (Generic Buffer Management) is the correct buffer allocator for VC4.
Instead of dumb buffers, the app allocates GBM surfaces and writes pixels via mmap.

```
drm crate  → open /dev/dri/cardN, find connector, set mode
gbm crate  → allocate GBM buffer surface
               └─ mmap → write RGBA pixels directly
drm crate  → add_framebuffer, page_flip
evdev crate → keyboard input (replaces SDL2 event pump)
```

**Pros**
- Removes SDL2 entirely
- Runtime deps: libdrm2 (~200 KB) + libgbm1 (~200 KB) — vs libsdl2 (~1.5 MB)
- RSS at idle: ~1–2 MB vs ~6–8 MB for SDL2
- Per frame: one fewer copy layer (no SDL2 texture upload)
- No SDL2 event loop overhead at idle

**Cons**
- ~300+ lines of additional unsafe/FFI code
- Must implement event input separately (`evdev` crate)
- `gbm` Rust crate is less mature than `drm`
- More brittle to Pi OS updates that change driver behaviour

Rust crates needed:
- [`drm`](https://crates.io/crates/drm) v0.15
- [`gbm`](https://crates.io/crates/gbm) v0.15
- [`evdev`](https://crates.io/crates/evdev) v0.12+

**Verdict: viable, meaningful gain only on Pi Zero.**

---

### Option D — SDL2 + DRM probe (currently implemented)

A hybrid: SDL2 does all rendering, but the `drm` crate is used once at startup
to probe the display hardware and configure SDL2 correctly.

```
Startup (once):
  drm crate → scan /dev/dri/card0..3
            → find connected HDMI/DSI connector
            → read native resolution from preferred DRM mode
            → set SDL_VIDEO_KMSDRM_DEVICE env var

Runtime:
  SDL2 (kmsdrm backend) → renders all frames
```

This fixes a real Pi 4/5 bug: the VC4 display engine is on `card1` but SDL2
defaults to `card0`, resulting in a silent black screen.

**Pros**
- Fixes card detection on Pi 4/5 automatically
- Native resolution auto-detected — no need to set width/height in config
- Minimal added complexity (~50 lines)
- No new runtime library beyond `libdrm2` (200 KB)
- Keeps working SDL2 rendering stack

**Cons**
- SDL2 memory and CPU overhead remain

**Verdict: currently implemented. Best balance of correctness and simplicity.**

---

## CPU and memory overhead comparison

### Memory (RSS at idle — photo on screen, no transition)

| Component | SDL2 + DRM probe | Direct DRM+GBM |
|---|---|---|
| Renderer library in RAM | ~4 MB (libsdl2) | ~0.4 MB (libdrm2 + libgbm1) |
| SDL2 internals (event loop, texture cache, renderer state) | ~2–3 MB | — |
| Our application code | ~2 MB | ~2 MB |
| **Total RSS** | **~8–12 MB** | **~4–6 MB** |

On Pi Zero (512 MB RAM) the ~5 MB saving is significant — it equals the RAM
needed to hold one decoded 720p photo.

### CPU overhead per transition frame

Each frame during a fade or slide transition involves:

| Step | SDL2 path | Direct DRM+GBM path |
|---|---|---|
| Decode JPEG | same | same |
| Lanczos scale | same | same |
| RGBA → display buffer | RGBA → SDL_Texture (lock+copy) → SDL_Renderer → EGL surface → GBM buffer | RGBA → GBM buffer (mmap write) |
| Present | SDL2 page flip via EGL | DRM page flip ioctl |
| **Extra full-frame copies** | **2–3** | **0** |

At 1280×720 (Pi Zero typical): each full-frame copy is ~3.7 MB.
SDL2 adds ~7–11 MB of extra memory bandwidth per frame during transitions.

At 15 fps / 800 ms fade: that's ~12 frames × 7–11 MB = ~100 MB of avoidable
memory traffic per transition. On Pi Zero's shared LPDDR2 bus, this is measurable.

### CPU at idle

| | SDL2 | Direct DRM+GBM |
|---|---|---|
| Event pump (SDL2 internal) | polls ~every 50 ms, ~1–2% CPU | not present |
| Tokio sleep loop | same | same |
| **Idle CPU** | **~1–2%** | **~0%** |

### Verdict by device

| Device | RAM | Cores | Recommended approach | Reason |
|---|---|---|---|---|
| Pi Zero W / 2W | 512 MB | 1 | Direct DRM+GBM | Every MB and CPU % counts |
| Pi 2 / 3 | 1 GB | 4 | Either | SDL2 overhead is small relative to headroom |
| Pi 4 / 5 | 4–8 GB | 4 | SDL2 + DRM probe | No reason to add complexity |

---

## Pixel path comparison (detailed)

```
── SDL2 + DRM probe (current) ──────────────────────────────────────

image::load_from_memory()
    │  decode JPEG/PNG/WebP
    ▼
fast_image_resize (Lanczos3)
    │  scale to screen size
    ▼
RgbaImage (Vec<u8>, RGBA layout)
    │
    ▼  SDL_LockTexture / with_lock
SDL2 Texture (internal SDL pixel buffer, may reformat)
    │
    ▼  SDL_RenderCopy
SDL2 Renderer (EGL-backed surface)
    │
    ▼  eglSwapBuffers
EGL surface → GBM buffer
    │
    ▼  DRM page flip
/dev/dri/cardN → HDMI


── Direct DRM+GBM (Option C) ────────────────────────────────────────

image::load_from_memory()
    │  decode JPEG/PNG/WebP
    ▼
fast_image_resize (Lanczos3)
    │  scale to screen size
    ▼
RgbaImage (Vec<u8>, RGBA layout)
    │
    ▼  mmap write (channel-swap R↔B for XRGB8888)
GBM buffer
    │
    ▼  DRM page flip ioctl
/dev/dri/cardN → HDMI
```

---

## DRM/KMS on Raspberry Pi — known specifics

| Pi model | GPU driver | DRM device | Dumb buffers | Notes |
|---|---|---|---|---|
| Pi Zero / 1 | vc4 | /dev/dri/card0 | No | Single card |
| Pi 2 / 3 | vc4 | /dev/dri/card0 | No | Single card |
| Pi 4 | vc4 (display) + v3d (3D) | card0 = v3d, **card1 = vc4** | No | Must use card1 for display |
| Pi 5 | rp1 + v3d | card0 = v3d, **card1 = rp1** | TBD | Check `DRM_CAP_DUMB_BUFFER` |

The Pi 4/5 multi-card layout is the main reason the DRM probe was added —
SDL2 silently opens `card0` (the 3-D engine) and shows nothing on HDMI.

**Groups required:** user must be in `video` and `render` groups to access
`/dev/dri/card*` without root. The installer adds these automatically.

---

## Future: implementing direct DRM+GBM

If Pi Zero becomes the primary target and the extra ~5 MB RSS / ~100 MB/transition
memory bandwidth needs to be eliminated, the implementation path is:

1. Add `gbm = "0.15"` and `evdev = "0.12"` to `Cargo.toml`, remove `sdl2`
2. Rewrite `src/renderer.rs`:
   - `open_card()` — scan `/dev/dri/card*`, probe via drm crate
   - `find_display()` — connector + CRTC + preferred mode
   - `GbmRenderer` struct — holds card, GBM device, double-buffered GBM bos
   - `flip()` — mmap write + `add_framebuffer` + `page_flip`
   - `spawn_input_thread()` — scan `/dev/input/event*` via evdev crate
3. Remove `libsdl2-dev`, `libsdl2-2.0-0` from `install.sh`; add `libgbm-dev`, `libgbm1`
4. Remove `sdl2` from `Cargo.toml`

Estimated effort: ~300 lines of new renderer code, ~50 lines removed.
No changes needed to `slideshow.rs`, `cache.rs`, `config.rs`, or any plugin.

---

*Last updated: 2026-04-12*
*Decision: Option D (SDL2 + DRM probe) implemented. Option C documented for future Pi Zero optimisation.*
