# PicoGallery Plan

Active work and known debt. Update checkboxes as items land. Requirements live
in `spec.md`; agent conventions in `AGENTS.md`.

## In flight (uncommitted on `main`; large dirty tree)

A gallery-thumbnail + plugin-lifecycle change is currently working-tree only.
**Nothing below is committed** — treat `main` as dirty until it is.

- [x] `core/src/thumb.rs` — prefer the embedded EXIF IFD1 JPEG thumbnail for gallery cells, re-injecting IFD0 Orientation so portrait photos still rotate. Gated to the gallery thumb path only: small LCDs pass display size into `get_photo_bytes`, so treating that as a thumb request would upscale a 160×120 stub.
- [x] `PhotoPlugin::refresh_auth` → `PhotoPlugin::shutdown` — plugins now get a graceful abort hook on switch/exit instead of a daily token-refresh callback.
- [x] `PluginConfig::apply_env_secret` returns `Result<bool>` (applied vs. absent) instead of `Result<()>`; set-but-empty stays an error.
- [x] All seven plugins updated for the trait change; `rust-version = 1.82` pinned; `tempfile` added as a workspace dev dependency.
- [ ] **Commit it — atomically.** The large working tree sitting on `main` is
  the single largest risk here. Check the live count with `git status --short`
  instead of copying a volatile number into this plan. `core/src/thumb.rs` is
  **untracked** while the modified `core/src/lib.rs` declares `pub mod thumb;`.
  `HEAD` is fine (its committed `lib.rs` has no such declaration), but staging
  `lib.rs` without `thumb.rs` produces a tree that does not compile. Stage both
  together.

## Immediate cleanup

- [ ] **Delete the root scratch files — they are already-landed duplicates.** `scratch_directory_tests.rs` (7 tests), `scratch_local_tests.rs` (8), `scratch_usb_tests.rs` (5), and `temp_tests.txt` (identical to the directory one) are leftover working copies; their test functions (`test_expand_home`, `test_list_albums`, `test_build_photo_list_at_ordering`, `test_get_photo_bytes_success`, …) already exist inside `plugins/*/src/lib.rs`. `scratch.py` is a one-off regex script over `src/slideshow.rs`. Nothing here needs folding in — just delete. They are untracked but **not gitignored**, so the next `git add -A` commits all five.
- [x] `.cursor/` is host-local and ignored alongside `.claude/` (2026-08-08).

## Bugs

- [ ] **`picogallery.service` at the repo root points at the wrong binary path.** The checked-in unit has `ExecStart=/usr/bin/picogallery`, but `install.sh` installs the binary to `/usr/local/bin/picogallery` and generates its own unit with `ExecStart=/usr/local/bin/picogallery` (install.sh:715). `install.sh:414` still copies the stale root unit into the extract dir. Anyone installing the checked-in unit by hand gets a service that fails to start. Fix the path in `picogallery.service`, or delete it and let `install.sh` be the only source of the unit.

- [ ] **Name collision with the sibling `pico-gallery-photoprism` project.** Both ship `picogallery`-prefixed systemd units, binaries, and config paths; they cannot currently coexist on one Pi without confusion. Tracked at the portfolio level in `../plan.md`.

## Known debt

- [ ] **No coverage measurement** (no tarpaulin/llvm-cov configured). The
  default workspace run currently lists **249 tests** (`cargo test --workspace
  -- --list`); nearly all are inline `#[cfg(test)]` rather than in `tests/`.
  `tests/` holds one integration test (`config_roundtrip.rs`), which is a
  layout choice, not a gap. Without a coverage number, the open question is
  *which* branches are missed, not whether tests exist.
- [ ] **No structured error taxonomy.** Everything is `anyhow`, which is right for the binary but makes "is this retryable?" a per-call-site judgement in plugin code.
- [ ] **No log file, journal only.** Fine on a supervised Pi; means a field failure is unrecoverable if the journal has rotated. Consider a bounded ring-buffer file if remote diagnosis becomes a need.
- [x] **Design notes are clone-visible (2026-08-08).** `plans/` is no longer
  ignored and has an index that marks proposals and rejected architectures as
  historical. The root `plan.md` remains the only active implementation plan.

## Verification before any commit

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./run.sh --no-launch          # full build + test with fixtures
```

Then a real-hardware smoke test: deploy, `sudo journalctl -u picogallery -n 50
--no-pager`, confirm a slide advances and `GET /api/health` answers.
