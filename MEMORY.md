# MEMORY — PicoGallery

Durable decisions and hard-won gotchas. Append when something is learned that
the code does not explain. Newest first. Keep entries short; if one grows past a
paragraph it belongs in `spec.md` or a module doc comment.

## Architectural decisions

- **Single-threaded Tokio, forever.** `main()` builds `tokio::runtime::Builder::new_current_thread()` after `prepare_display_env()`, and the tokio feature list omits `rt-multi-thread` so the work-stealing scheduler is not even linked. The Pi Zero 2 W has one usable core for this workload; a second scheduler is pure overhead. Same reason `jpeg-decoder` is `default-features = false` (no rayon).
- **`opt-level = 2`, not `"z"`.** Counterintuitive for an embedded target, but this is compute-bound (JPEG decode → `fast_image_resize` → qcms, per slide). `"z"` disables the inlining and vectorization those loops depend on. `lto = true` buys back most of the size.
- **Plugins depend on `core`, never on the root crate.** That direction is what keeps the workspace acyclic and lets a new source be added without touching engine code.
- **Metadata/bytes split.** `PhotoMeta` carries no pixels; `get_photo_bytes` fetches on demand. This is what makes a 20k-photo library viable in 384 MB.
- **Raw `TcpListener` for the remote.** A framework would cost binary size and idle wakeups for six endpoints. One parked accept task is the whole cost.
- **Night mode is 8.8 fixed-point, applied per slide.** No floats, no division, no per-pixel bounds checks. Per *frame* would be unaffordable; per *slide* is a few ms.
- **Favourite toggle is a metadata write, not a library edit.** Spec §7 forbids pixel write/delete. PhotoPrism `set_favorite` POSTs/DELETEs `/photos/{uid}/like` only when the plugin advertises `favorite_toggle`. Directory/WebDAV do not.

## Gotchas

- **`env_logger` precedence is resolved by clap, not by env.** `--log-level` beats `RUST_LOG` beats `info`, and the resolved string is passed straight to `parse_filters`. It deliberately does not round-trip through `std::env::set_var`, which is racy and `unsafe` as of Rust 2024. Do not "simplify" this.
- **EXIF thumbs are gallery-only.** Small LCDs (480×320) pass their display size into `get_photo_bytes`. If that were treated as a thumb request, a 160×120 IFD1 stub would get upscaled to fullscreen. `prefer_gallery_exif_thumb` must only be called from the gallery thumb path.
- **Using an EXIF thumb loses IFD0 Orientation** unless it is re-injected into the returned JPEG — otherwise portrait photos display sideways in the grid only.
- **`.cargo/` must stay uncommitted.** A host-local `[build] target` (e.g. forcing x86_64 on Apple Silicon with Intel-only Homebrew) silently breaks native Linux, Pi, and CI builds for everyone else.
- **`plans/` is a tracked archive, not the active plan.** Its `README.md`
  distinguishes research, proposals, and rejected architectures. Active work
  belongs only in the root `plan.md`; host-local agent state remains ignored.
- **The cache key `{plugin_name}/{id}` is a compatibility surface.** Change it and every deployed device re-downloads its entire library over the network.
- **Wi-Fi and USB mounting are not privileges the unit holds.** They are delegated to host policy (NetworkManager / udisks). "Just add a capability" is the wrong fix when those paths fail.

## Operational

- Logs go to the systemd journal only — there is no log file. `sudo journalctl -u picogallery -f`.
- `MemoryMax=384M` + `Restart=on-failure`: a silent restart loop looks like an app bug but is often the OOM killer. Check the journal for the kill line.
- `deploy.sh` installs a pre-built GitHub release binary; the Pi never compiles Rust.
