# AGENTS.md — PicoGallery

Single source of truth for coding agents. `CLAUDE.md` is a symlink to this file.
User-facing setup lives in `README.md`. This file records only what an agent
cannot infer from the code.

## What this is

Plugin-based photo slideshow for a **Raspberry Pi Zero 2 W with no desktop
environment**. Renders straight to KMS/DRM via SDL2. Seven source plugins exist;
six are in `default` (directory, local, USB, WebDAV, PhotoPrism, Google Photos)
and **`amazon-photos` is opt-in** — a stock build does not include it.
Every design decision below exists because the target has a **one-core CPU
budget and ~512 MB RAM** — that constraint outranks general "good Rust"
instincts.

## Stack & layout

- Cargo **workspace**: root crate `picogallery` + `core` + `plugins/*`. Edition 2021, `rust-version = 1.82`.
- `core/` — `PhotoPlugin` trait, `PhotoMeta`, `PluginConfig`, EXIF thumb helpers. **Plugins depend on `core`, never on the root crate** (that is what keeps the graph acyclic).
- `src/main.rs` — CLI (clap), logger init, plugin registry, startup.
- `src/renderer.rs` — SDL2. Linux: probes `/dev/dri/card*` via the `drm` crate for the card + native resolution, then uses SDL's `kmsdrm` driver. macOS: native Cocoa/Metal backend for dev.
- `src/slideshow.rs` — Tokio task that pre-fetches the next N images while the current one is on screen.
- `src/gallery.rs` + `gallery_controller.rs` — thumbnail grid. `src/fullscreen_controller.rs` — deferred "open this index" request. `src/mode.rs` — `Mode::{Gallery, Fullscreen}`.
- `src/cache.rs` — disk LRU, `<cache_dir>/<sanitised_key>-<fnv1a>.jpg` + `index.json`, rescanned on startup. O(1) get/put via slab-backed intrusive list. `CacheHandle` degrades to no-cache if the dir is unwritable.
- `[auth]` config (`pending_timeout_secs`), `src/fetcher.rs` (display-loop I/O offload). `main()` sets SDL env then builds a `current_thread` runtime (no `#[tokio::main]`).
- `src/remote.rs` — phone remote on a **raw `TcpListener`** (no HTTP framework). `GET /`, `POST /api/{next,prev,pause,favorite}`, `GET /api/{status,health}`.
- `src/night.rs` — dim + warm tint in one 8.8 fixed-point pass, once per slide (never per frame).
- Also: `config.rs`, `compose.rs`, `osd.rs`, `menu.rs`, `wifi.rs`, `cec_remote.rs`, `display_power.rs`, `exif_util.rs`.

## Commands

```bash
./run.sh                       # build + test + generate fixtures in /tmp/picogallery-e2e + launch
./run.sh --photos /path/to/dir # use a real library (skips fixture gen and config overwrite)
./run.sh --no-launch           # build + test only
./run.sh --log-level debug     # extra flags pass through to the binary

cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Config: `~/.config/picogallery/config.toml`.
`--print-default-config` dumps the template; `--generate-config [--force]` writes it.

Deployment scripts: `install.sh` (provision a Pi), `deploy.sh` (drop a pre-built
GitHub release binary — no on-device compile), `release.sh`, `uninstall.sh`.

## Logging

- `env_logger`. Precedence is resolved by clap and is **`--log-level` flag > `RUST_LOG` env > `info`** — `main.rs` feeds the resolved value straight to `env_logger::Builder::parse_filters`, deliberately *not* via `std::env::set_var` (racy/unsafe in Rust 2024). Do not "simplify" that back.
- There is **no log file**. Under systemd, `StandardOutput=journal` / `StandardError=journal`, so the journal is the only sink. In dev, it is stderr.

## How to debug

```bash
sudo journalctl -u picogallery -f              # live
sudo journalctl -u picogallery -n 50 --no-pager # last 50 (what install.sh tells operators)
sudo systemctl restart picogallery
RUST_LOG=debug ./run.sh --no-launch            # local, verbose
```

- "No enabled provider initialized successfully" → every plugin failed to init; the per-plugin cause is one of the preceding journal lines.
- Black screen on the Pi → DRM/permissions first. The installed service runs
  as the configured user (normally `pi`) with `Group=video`,
  `SupplementaryGroups=video render input`, and `SDL_VIDEODRIVER=kmsdrm`; if
  `/dev/dri/card*` probing fails, `renderer.rs` logs it at `warn`.
- OOM / silent restarts → the unit caps `MemoryMax=384M` and `TasksMax=128` with `Restart=on-failure`. Check the journal for the kill, not just the app log.
- Config not taking effect → confirm which file was loaded; startup logs `Loading config from <path>`.

## Graphify — predates the current working tree

A hook may prompt you to read `graphify-out/GRAPH_REPORT.md` before searching.
It was built 2026-07-31 and its report records **no commit hash**, so staleness
is not self-evident. The large uncommitted change in this tree — including
untracked `core/src/thumb.rs` — postdates it and is **absent from the graph**.
Treat "not in the graph" as "not yet committed", never as "does not exist", and
confirm anything load-bearing against the file. `graphify-out/` is gitignored,
so it is local-only.

## Error handling

- `anyhow::Result` everywhere, with `.context()` at I/O and config boundaries. Reserve `Option` for "absent", not "failed".
- **Plugin failures are isolated, not fatal**: one provider failing to init must not take the display down. Only an empty enabled-provider set is a startup error. Preserve that.
- Never `.unwrap()` on a plugin, network, or filesystem result in a display-loop path — a panic here is a bricked photo frame that only `Restart=on-failure` recovers.
- Secrets stay out of `PhotoMeta.extra` (it is serialized to cache); session credentials and bearer tokens live in plugin session state, and `config.rs` strips inline secrets before writing `config.toml` back.

## Conventions

- Adding a photo source = a new crate under `plugins/`, a workspace member, a `dep:` feature flag in root `[features]`, and a branch in `build_plugins()`. Nothing in `core` changes.
- Cache key is `{plugin_name}/{id}` via `PhotoMeta::cache_key` — keep it stable or every device re-downloads its whole library.
- New tunables go in `config.toml` + `config.example.toml`, not inline constants.
- `plans/` contains tracked historical research and proposals; its `README.md`
  labels what is current versus archival. `graphify-out/`, `mnt/`, `.cargo/`,
  `.claude/`, and `.cursor/` are host-local and gitignored.

## Do NOT

- Do NOT add `rt-multi-thread` to the tokio features. `main()` builds `tokio::runtime::Builder::new_current_thread()` after SDL env setup because the Pi Zero has one core; the feature exists only to keep the work-stealing scheduler out of the binary.
- Do NOT enable `rayon` (e.g. via `jpeg-decoder` default features). It is off deliberately to keep the single core available for the display loop.
- Do NOT set `[profile.release] opt-level = "z"`. This is compute-bound (JPEG decode + `fast_image_resize` + qcms per slide); `z` kills the inlining/vectorization those loops need. `lto = true` already recovers most of the size.
- Do NOT commit `.cargo/` — a host-local `[build] target` there breaks native Linux / Pi / CI builds.
- Do NOT commit `config.toml`, `deploy.local.env`, or anything matching `*credentials*` / `client_secret*` / `*.pem` / `*.key`.
- Do NOT make `core` depend on the root crate — it inverts the plugin dependency direction.
