/// Renderer — SDL2 with KMS/DRM backend (Linux) or native backend (macOS/dev).
///
/// On Linux: probes `/dev/dri/card*` via the `drm` crate at startup to find the
/// correct card and native resolution, then hands that to SDL2's kmsdrm driver.
/// On macOS/other: SDL2 uses its native backend (Cocoa/Metal) automatically.
use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};
use log::{info, warn};
use sdl2::{
    event::Event,
    keyboard::Keycode,
    mouse::MouseButton,
    pixels::PixelFormatEnum,
    rect::Rect,
    render::{Canvas, Texture, TextureCreator},
    video::{Window, WindowContext},
    EventPump, Sdl,
};
use std::io::Cursor;
use std::time::{Duration, Instant};

use crate::config::DisplayConfig;

// ── DRM display probe (Linux only) ───────────────────────────────────────────

#[cfg(target_os = "linux")]
mod drm_probe {
    use drm::control::{connector, Device as ControlDevice};
    use drm::Device;
    use log::{info, warn};
    use std::os::unix::io::{AsFd, BorrowedFd, RawFd};

    pub struct DrmCard(pub std::fs::File);
    impl AsFd for DrmCard {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl Device for DrmCard {}
    impl ControlDevice for DrmCard {}

    /// Scan /dev/dri/card0..3 and return (device_path, width, height) for the
    /// first card that has a connected display. Returns None if nothing found.
    ///
    /// Emits verbose diagnostics at info-level so users can see exactly why a
    /// probe failed: missing /dev/dri, permission denied, no connectors, no
    /// connected state, etc. Accepts connectors whose state is `Unknown` but
    /// which advertise modes — some Pi DRM drivers don't populate the
    /// `Connected` state reliably.
    pub fn probe() -> Option<(String, u32, u32)> {
        use std::os::unix::fs::PermissionsExt;

        // 1. List /dev/dri so we know whether the kernel even exposes DRM.
        match std::fs::read_dir("/dev/dri") {
            Ok(entries) => {
                let names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let mode = e
                            .metadata()
                            .map(|m| m.permissions().mode() & 0o777)
                            .unwrap_or(0);
                        format!("{} (mode {:o})", name, mode)
                    })
                    .collect();
                info!("DRM probe: /dev/dri contains: [{}]", names.join(", "));
            }
            Err(e) => {
                warn!(
                    "DRM probe: cannot read /dev/dri — {} (is the kernel module loaded?)",
                    e
                );
                return None;
            }
        }

        for n in 0..4 {
            let path = format!("/dev/dri/card{}", n);

            if !std::path::Path::new(&path).exists() {
                continue;
            }

            // Open read+write: some DRM drivers refuse resource enumeration on
            // read-only fds. Requires the `video` group either way.
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(f) => f,
                Err(e) => {
                    warn!(
                        "DRM probe: cannot open {} — {} (is the current user in the `video` group?)",
                        path, e
                    );
                    continue;
                }
            };

            let card = DrmCard(file);
            let res = match card.resource_handles() {
                Ok(r) => r,
                Err(e) => {
                    warn!("DRM probe: {} no resource handles — {}", path, e);
                    continue;
                }
            };
            info!(
                "DRM probe: {} has {} connector(s), {} crtc(s)",
                path,
                res.connectors().len(),
                res.crtcs().len()
            );

            for &conn_h in res.connectors() {
                let info = match card.get_connector(conn_h, false) {
                    Ok(i) => i,
                    Err(e) => {
                        info!("DRM probe:   connector {:?} err: {}", conn_h, e);
                        continue;
                    }
                };
                let state = info.state();
                let iface = info.interface();
                let n_modes = info.modes().len();
                info!(
                    "DRM probe:   {:?} state={:?} modes={}",
                    iface, state, n_modes
                );

                // Accept Connected, or Unknown-with-modes (some Pi drivers
                // never set Connected even when HDMI is live).
                let usable = matches!(state, connector::State::Connected)
                    || (matches!(state, connector::State::Unknown) && n_modes > 0);
                if !usable {
                    continue;
                }

                if let Some(mode) = info.modes().first() {
                    let (w, h) = (mode.size().0 as u32, mode.size().1 as u32);
                    info!(
                        "DRM probe: SELECTED {} — {:?} state={:?} native {}×{}",
                        path, iface, state, w, h
                    );
                    return Some((path, w, h));
                }
            }
        }
        warn!("DRM probe: no usable display found in /dev/dri/card0..3 — check HDMI cable, monitor power, and that the current user is in the `video` group");
        None
    }
}

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    // Kept alive for the renderer's lifetime; never read directly —
    // events come from `event_pump`.
    _sdl_ctx: Sdl,
    event_pump: EventPump,
    canvas: Canvas<Window>,
    width: u32,
    height: u32,
    config: DisplayConfig,
    /// SDL text-input handle. Enabled only while a settings-menu field is being
    /// edited, so the rest of the time single-key shortcuts (q/p/f/m) still fire
    /// as keys rather than being swallowed as typed text.
    text_input: sdl2::keyboard::TextInputUtil,
    text_input_active: bool,
    /// Precached sRGB output profile for ICC colour correction. Built once
    /// (the LUT precompute is the costly part) and reused for every photo —
    /// only the per-image *input* profile is rebuilt. Saves rebuilding the
    /// sRGB lookup tables on each slide, which matters on a Pi Zero.
    srgb_profile: Box<qcms::Profile>,
}

impl Renderer {
    pub fn init(config: DisplayConfig) -> Result<Self> {
        // ── XDG_RUNTIME_DIR (Linux only) ──────────────────────────────────────
        // SDL2 / mesa / dbus all complain when this is unset under systemd or
        // when running over SSH. Point it at /run/user/$UID if that exists
        // (systemd-logind creates it for interactive sessions) or fall back
        // to a private /tmp/runtime-$UID dir with 0700 perms.
        #[cfg(target_os = "linux")]
        {
            if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
                let uid: u32 = std::fs::read_to_string("/proc/self/status")
                    .ok()
                    .and_then(|s| {
                        s.lines()
                            .find(|l| l.starts_with("Uid:"))
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|v| v.parse::<u32>().ok())
                    })
                    .unwrap_or(1000);
                let run_user = format!("/run/user/{}", uid);
                let chosen = if std::path::Path::new(&run_user).is_dir() {
                    run_user
                } else {
                    let fallback = format!("/tmp/runtime-{}", uid);
                    if let Err(e) = std::fs::create_dir_all(&fallback) {
                        warn!("Could not create {}: {}", fallback, e);
                    } else {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &fallback,
                            std::fs::Permissions::from_mode(0o700),
                        );
                    }
                    fallback
                };
                info!("Setting XDG_RUNTIME_DIR={}", chosen);
                std::env::set_var("XDG_RUNTIME_DIR", chosen);
            }
        }

        // ── DRM display probe (Linux only) ────────────────────────────────────
        // Finds the correct /dev/dri/cardN (Pi 4/5 has display on card1, not card0)
        // and the native resolution. On macOS this block is compiled away entirely.
        #[cfg(target_os = "linux")]
        let (probed_w, probed_h, probed_path) = {
            if let Some((dev_path, w, h)) = drm_probe::probe() {
                (w, h, Some(dev_path))
            } else {
                (0u32, 0u32, None)
            }
        };
        #[cfg(not(target_os = "linux"))]
        let (probed_w, probed_h, _probed_path) = (0u32, 0u32, None::<String>);

        // ── KMS/DRM backend selection (Linux only) ────────────────────────────
        #[cfg(target_os = "linux")]
        {
            let env_driver = std::env::var("SDL_VIDEODRIVER").ok();
            let has_x11 = std::env::var("DISPLAY").is_ok();
            let has_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

            // Use kmsdrm if:
            //   a) the caller/service already requested it explicitly, OR
            //   b) no graphical session is present and no driver override was given
            let want_kmsdrm = match env_driver.as_deref() {
                Some("kmsdrm") => true,
                None if !has_x11 && !has_wayland => true,
                _ => false,
            };

            if want_kmsdrm {
                if env_driver.is_none() {
                    info!("No graphical session detected; forcing SDL_VIDEODRIVER=kmsdrm");
                    std::env::set_var("SDL_VIDEODRIVER", "kmsdrm");
                }
                // Always apply the probed device so SDL picks the right card
                // (Pi 4 has the display on card1, not card0 which SDL defaults to).
                if std::env::var("SDL_VIDEO_KMSDRM_DEVICE").is_err() {
                    if let Some(ref path) = probed_path {
                        info!("Setting SDL_VIDEO_KMSDRM_DEVICE={}", path);
                        std::env::set_var("SDL_VIDEO_KMSDRM_DEVICE", path);
                    }
                }
            }
        }

        let sdl_ctx = sdl2::init().map_err(|e| anyhow::anyhow!("SDL init: {}", e))?;

        // Try to bring up the video subsystem. If kmsdrm was requested and fails,
        // log every SDL video driver that *is* available and retry without the
        // SDL_VIDEODRIVER override so SDL can pick its own default (fbdev/etc).
        let video = match sdl_ctx.video() {
            Ok(v) => v,
            Err(e) => {
                let requested = std::env::var("SDL_VIDEODRIVER").ok();
                let num = unsafe { sdl2::sys::SDL_GetNumVideoDrivers() };
                let drivers: Vec<String> = (0..num)
                    .filter_map(|i| unsafe {
                        let ptr = sdl2::sys::SDL_GetVideoDriver(i);
                        if ptr.is_null() {
                            None
                        } else {
                            Some(std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned())
                        }
                    })
                    .collect();
                warn!(
                    "SDL video init failed with SDL_VIDEODRIVER={:?}: {}",
                    requested, e
                );
                warn!(
                    "SDL was compiled with these video drivers: [{}]",
                    drivers.join(", ")
                );

                // Retry only if kmsdrm was the problem and SDL has *some* other driver.
                #[cfg(target_os = "linux")]
                {
                    if requested.as_deref() == Some("kmsdrm") && drivers.len() > 1 {
                        warn!("Retrying SDL video init without SDL_VIDEODRIVER override...");
                        std::env::remove_var("SDL_VIDEODRIVER");
                        std::env::remove_var("SDL_VIDEO_KMSDRM_DEVICE");
                        sdl_ctx
                            .video()
                            .map_err(|e2| anyhow::anyhow!("SDL video (fallback): {}", e2))?
                    } else {
                        return Err(anyhow::anyhow!("SDL video: {}", e));
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    return Err(anyhow::anyhow!("SDL video: {}", e));
                }
            }
        };

        // Log the driver SDL actually chose. If it fell back to `dummy` or
        // `offscreen`, the window will create successfully but nothing will
        // ever render — bail out with a clear message instead of silently
        // running a no-op slideshow.
        let active_driver = unsafe {
            let ptr = sdl2::sys::SDL_GetCurrentVideoDriver();
            if ptr.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        info!("SDL video driver in use: {}", active_driver);
        if matches!(active_driver.as_str(), "dummy" | "offscreen") {
            let have_dri = std::path::Path::new("/dev/dri").exists();
            let hint = if !have_dri {
                "/dev/dri does not exist — the kernel DRM module isn't loaded. \
                 On Raspberry Pi (especially DietPi/minimal images) add this to \
                 /boot/firmware/config.txt (or /boot/config.txt on older releases) \
                 and reboot:\n\
                 \n    dtoverlay=vc4-kms-v3d\n    max_framebuffers=2\n"
            } else {
                "/dev/dri exists but SDL couldn't open a device. Run `ls -l /dev/dri` \
                 and confirm the current user is in the `video` group, that HDMI is \
                 connected, and that libgbm1 + libegl1 are installed."
            };
            return Err(anyhow::anyhow!(
                "SDL fell back to the '{}' driver — no real display backend is available.\n{}",
                active_driver,
                hint
            ));
        }

        // ── Resolution: config > DRM probe > SDL2 desktop query ───────────────
        let (w, h) = if config.width > 0 && config.height > 0 {
            (config.width, config.height)
        } else if probed_w > 0 {
            (probed_w, probed_h)
        } else {
            let dm = video
                .desktop_display_mode(0)
                .map_err(|e| anyhow::anyhow!("display mode: {}", e))?;
            (dm.w as u32, dm.h as u32)
        };
        info!("Display: {}×{}", w, h);

        let window = video
            .window("picogallery", w, h)
            .fullscreen_desktop()
            .position_centered()
            .build()
            .context("creating SDL window")?;

        let mut canvas = window
            .into_canvas()
            .accelerated()
            .present_vsync()
            .build()
            .context("creating SDL canvas")?;

        canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        canvas.clear();
        canvas.present();

        // Create the event pump once — recreating it on every 50 ms poll tick
        // costs a lock + alloc inside SDL for no benefit.
        let event_pump = sdl_ctx
            .event_pump()
            .map_err(|e| anyhow::anyhow!("SDL event pump: {}", e))?;

        // Text input starts on for some SDL backends — turn it off until a menu
        // field is actually being edited, so typed letters don't shadow the
        // single-key shortcuts.
        let text_input = video.text_input();
        text_input.stop();

        // Build the sRGB output profile once; precompute its LUTs up front so
        // per-photo colour correction only builds the (per-image) input profile.
        let mut srgb_profile = qcms::Profile::new_sRGB();
        srgb_profile.precache_output_transform();

        Ok(Self {
            _sdl_ctx: sdl_ctx,
            event_pump,
            canvas,
            width: w,
            height: h,
            config,
            text_input,
            text_input_active: false,
            srgb_profile,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Replace the live display config. Used when settings change at runtime
    /// via the right-click menu — affects subsequent `decode_and_scale`
    /// (fill/letterbox/limits) and transition timing (fps). The window itself
    /// is not resized, so `width`/`height` overrides are ignored here.
    pub fn set_display_config(&mut self, config: DisplayConfig) {
        self.config = config;
    }

    // ── Image decode & scale ─────────────────────────────────────────────────

    /// Decode, EXIF-correct, and scale an image in one pass.
    ///
    /// Returns `(display_rgba, exif_date)`.
    ///
    /// # Memory strategy
    ///
    /// Photos are scaled to display resolution *first*, then the small
    /// display-sized image (~8 MB) is rotated.  This is cheaper than rotating
    /// the full-resolution original first. The scale step runs in RGB
    /// (3 bytes/px), so the decode buffer is the *only* full-resolution
    /// allocation — no full-res RGBA copy is ever made:
    ///
    /// ```text
    ///   scale-then-rotate  peak ≈ full-res-RGB + display-size  (e.g. 36+8 = 44 MB)
    ///   rotate-then-scale  peak ≈ full-res + full-res-copy     (e.g. 48+48 = 96 MB)
    /// ```
    ///
    /// For 90°/270° rotations the scale target dimensions are swapped
    /// (height↔width) so the scaler outputs the correct portrait/landscape
    /// size for the rotated result — same visual output, half the RAM.
    ///
    /// # Configurable safety limits
    ///
    /// Both limits are checked before the expensive decode step:
    /// - `max_image_mb`   — raw file size gate (default 50 MB)
    /// - `max_megapixels` — decoded pixel count gate (0 = built-in 24 MP backstop)
    pub fn decode_and_scale(&self, bytes: &[u8]) -> Result<(RgbaImage, Option<String>)> {
        // ── Raw-size gate ──────────────────────────────────────────────────────
        let max_bytes = if self.config.max_image_mb > 0 {
            self.config.max_image_mb as usize * 1_048_576
        } else {
            50 * 1_048_576 // default 50 MB
        };
        if bytes.len() > max_bytes {
            return Err(anyhow::anyhow!(
                "image file {} MB exceeds max_image_mb={} — skipping \
                 (set a higher limit or resize photos before uploading)",
                bytes.len() / 1_048_576,
                max_bytes / 1_048_576,
            ));
        }

        let is_jpeg = bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff;

        let mut icc_profile = None;
        let mut decoded_img = None;

        if is_jpeg {
            let cursor = Cursor::new(bytes);
            let options = zune_core::options::DecoderOptions::default()
                .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);

            let mut decoder = zune_jpeg::JpegDecoder::new_with_options(cursor, options);
            if decoder.decode_headers().is_ok() {
                if let Some(info) = decoder.info() {
                    let w = info.width;
                    let h = info.height;

                    // ── Hard dimension ceiling (OOM guard) ─────────────────────────
                    if w > 16_000 || h > 16_000 {
                        return Err(anyhow::anyhow!(
                            "image {}×{} px exceeds the 16 000-px dimension safety limit",
                            w,
                            h
                        ));
                    }

                    // ── Megapixel gate (configured value or built-in backstop) ──────
                    check_megapixels(self.config.max_megapixels, w as u32, h as u32)?;

                    match decoder.decode() {
                        Ok(pixels) => {
                            icc_profile = decoder.icc_profile();
                            if let Some(rgb_img) =
                                image::RgbImage::from_raw(w as u32, h as u32, pixels)
                            {
                                decoded_img = Some(image::DynamicImage::ImageRgb8(rgb_img));
                            }
                        }
                        Err(e) => {
                            warn!("zune-jpeg failed to decode jpeg body: {:?}, falling back to image crate", e);
                        }
                    }
                }
            }
        }

        let img = if let Some(d_img) = decoded_img {
            d_img
        } else {
            let img = image::load_from_memory(bytes).context("decoding image")?;

            // ── Hard dimension ceiling (OOM guard) ─────────────────────────────────
            if img.width() > 16_000 || img.height() > 16_000 {
                return Err(anyhow::anyhow!(
                    "image {}×{} px exceeds the 16 000-px dimension safety limit",
                    img.width(),
                    img.height()
                ));
            }

            // ── Megapixel gate (configured value or built-in backstop) ──────────────
            check_megapixels(self.config.max_megapixels, img.width(), img.height())?;
            img
        };

        // ── EXIF (single parse) ────────────────────────────────────────────────
        // Reads orientation + capture date in one Reader::read_from_container
        // pass — halves heap allocations vs two separate parses.
        let exif = crate::exif_util::read_exif(bytes);
        let orientation = exif.orientation;
        let exif_date = exif.date;

        // ── Scale, then rotate the small image ────────────────────────────────
        // Swap target dimensions for 90°/270° so the scaler sees the correct
        // target aspect ratio; the rotation brings it back to display orientation.
        let (target_w, target_h) = if matches!(orientation, 5..=8) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };

        let scaled = self.scale_image_to(img, target_w, target_h, &icc_profile)?;
        let oriented = crate::exif_util::apply_orientation_rgba(scaled, orientation);

        // ── Blurred letterbox fill ─────────────────────────────────────────────
        // When letterboxing (no crop) and the photo doesn't cover the screen,
        // composite it onto a full-screen blurred copy of itself instead of
        // black bars. One-shot cost of a few ms per slide: blur happens on a
        // 64-px-wide thumbnail using stackblur-iter; the GPU-friendly bilinear
        // upscale and a row-memcpy composite do the rest.
        let final_img = if self.config.letterbox_blur
            && !self.config.fill_screen
            && (oriented.width() < self.width || oriented.height() < self.height)
        {
            compose_letterbox_blur(&oriented, self.width, self.height)
        } else {
            oriented
        };

        Ok((final_img, exif_date))
    }

    // scale_image is kept for any future callers that don't need orientation.
    #[allow(dead_code)]
    fn scale_image(&self, img: DynamicImage) -> Result<RgbaImage> {
        self.scale_image_to(img, self.width, self.height, &None)
    }

    /// Scale `img` to fit (letterbox) or cover (fill_screen) a `dw×dh` target.
    ///
    /// Failures propagate as errors so the caller skips the photo — same
    /// handling as a decode error — instead of displaying a black frame.
    fn scale_image_to(
        &self,
        img: DynamicImage,
        dw: u32,
        dh: u32,
        icc_profile: &Option<Vec<u8>>,
    ) -> Result<RgbaImage> {
        let (sw, sh) = (img.width(), img.height());

        // fill_screen: cover (max scale, may crop); letterbox: contain (min scale, no crop).
        let scale = if self.config.fill_screen {
            f32::max(dw as f32 / sw as f32, dh as f32 / sh as f32)
        } else {
            f32::min(dw as f32 / sw as f32, dh as f32 / sh as f32)
        };
        let (nw, nh) = ((sw as f32 * scale) as u32, (sh as f32 * scale) as u32);

        use fast_image_resize as fir;
        let (sw_nz, sh_nz) = match (std::num::NonZeroU32::new(sw), std::num::NonZeroU32::new(sh)) {
            (Some(w), Some(h)) => (w, h),
            _ => {
                return Err(anyhow::anyhow!(
                    "cannot scale zero-sized image {}×{}",
                    sw,
                    sh
                ))
            }
        };
        // Resize in RGB (3 bytes/px), not RGBA. The resizer's *source* is the
        // full-resolution decode, so dropping the alpha plane avoids ever
        // allocating a full-res W×H×4 buffer purely as scaler input — the single
        // largest allocation per photo. For a typical JPEG (already Rgb8)
        // `into_rgb8()` is a move, not a copy. Alpha (always opaque for photos)
        // is added back once at the small display size below.
        //   12 MP example: peak ≈ 36 MB (RGB) vs the old ≈ 48 MB RGBA plus an
        //   ~84 MB transient spike while the RGB and RGBA buffers coexisted.
        let src = fir::Image::from_vec_u8(
            sw_nz,
            sh_nz,
            img.into_rgb8().into_raw(),
            fir::PixelType::U8x3,
        )
        .map_err(|e| anyhow::anyhow!("building resize source image: {}", e))?;

        let (dst_w, dst_h) = (nw.max(1), nh.max(1));
        let mut dst = fir::Image::new(
            std::num::NonZeroU32::new(dst_w).unwrap(),
            std::num::NonZeroU32::new(dst_h).unwrap(),
            fir::PixelType::U8x3,
        );

        let mut resizer = fir::Resizer::new(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3));
        resizer
            .resize(&src.view(), &mut dst.view_mut())
            .map_err(|e| anyhow::anyhow!("image resize: {}", e))?;

        // Expand the display-sized RGB result to the RGBA that SDL textures and
        // the orientation imageops require — one small (display-sized) copy.
        let mut rgb = image::RgbImage::from_raw(dst_w, dst_h, dst.into_vec())
            .context("resized pixel buffer has unexpected size")?;

        // ── Color Profile Correction ──────────────────────────────────────────
        // Only runs for images that carry an ICC profile; the sRGB output side
        // is precomputed once (see `srgb_profile`), so this is just the input
        // profile + a transform over the small display-sized buffer.
        if let Some(icc_data) = icc_profile {
            if let Some(input_profile) = qcms::Profile::new_from_slice(icc_data, false) {
                if let Some(transform) = qcms::Transform::new(
                    &input_profile,
                    &self.srgb_profile,
                    qcms::DataType::RGB8,
                    qcms::Intent::default(),
                ) {
                    transform.apply(&mut rgb);
                }
            }
        }

        Ok(DynamicImage::ImageRgb8(rgb).into_rgba8())
    }

    // ── Display methods ──────────────────────────────────────────────────────

    pub fn show_cut(&mut self, rgba: &RgbaImage) -> Result<()> {
        let tc = self.canvas.texture_creator();
        let tex = rgba_to_texture(&tc, rgba)?;
        self.canvas.clear();
        blit_centered(
            &mut self.canvas,
            &tex,
            rgba.width(),
            rgba.height(),
            self.width,
            self.height,
        )?;
        self.canvas.present();
        Ok(())
    }

    /// Async so the frame-budget sleep yields to the runtime: on the
    /// current_thread executor a `std::thread::sleep` here would starve every
    /// other task (HTTP remote, prefetch) for the whole transition. SDL stays
    /// on the main thread — the executor never migrates this future.
    pub async fn show_fade(
        &mut self,
        current_rgba: Option<&RgbaImage>,
        next_rgba: &RgbaImage,
        duration: Duration,
    ) -> Result<()> {
        if duration.is_zero() {
            return self.show_cut(next_rgba);
        }

        let tc = self.canvas.texture_creator();
        // Pre-bake current frame texture (outside the loop — avoids re-allocating every frame).
        let cur_tex = match current_rgba {
            Some(cur) => Some(rgba_to_texture(&tc, cur)?),
            None => None,
        };
        let mut next_tex = rgba_to_texture(&tc, next_rgba)?;
        // SDL_RenderCopy only respects set_alpha_mod when the texture blend mode is
        // SDL_BLENDMODE_BLEND.  Without this the alpha_mod writes transparent pixels
        // to the Metal framebuffer and macOS shows the white desktop behind the window.
        next_tex.set_blend_mode(sdl2::render::BlendMode::Blend);

        let cur_dims = current_rgba.map(|r| (r.width(), r.height()));
        let (nw, nh) = (next_rgba.width(), next_rgba.height());
        let budget = Duration::from_millis(1000 / self.config.fps.max(1) as u64);
        let inv_dur = 1.0 / duration.as_secs_f32();
        self.canvas
            .set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        let start = Instant::now();
        loop {
            let elapsed = start.elapsed();
            if elapsed >= duration {
                break;
            }

            let alpha = (elapsed.as_secs_f32() * inv_dur * 255.0) as u8;
            self.canvas.clear();

            if let (Some(ref cur), Some((cw, ch))) = (&cur_tex, cur_dims) {
                blit_centered(&mut self.canvas, cur, cw, ch, self.width, self.height)?;
            }
            next_tex.set_alpha_mod(alpha);
            blit_centered(&mut self.canvas, &next_tex, nw, nh, self.width, self.height)?;
            self.canvas.present();

            let frame_time = start.elapsed() - elapsed;
            if frame_time < budget {
                tokio::time::sleep(budget - frame_time).await;
            }
        }

        // Final frame: reuse the pre-baked texture instead of re-uploading via show_cut.
        next_tex.set_alpha_mod(255);
        self.canvas.clear();
        blit_centered(&mut self.canvas, &next_tex, nw, nh, self.width, self.height)?;
        self.canvas.present();
        Ok(())
    }

    pub async fn show_slide_left(
        &mut self,
        current_rgba: Option<&RgbaImage>,
        next_rgba: &RgbaImage,
        duration: Duration,
    ) -> Result<()> {
        self.show_slide(current_rgba, next_rgba, duration, true)
            .await
    }

    pub async fn show_slide_right(
        &mut self,
        current_rgba: Option<&RgbaImage>,
        next_rgba: &RgbaImage,
        duration: Duration,
    ) -> Result<()> {
        self.show_slide(current_rgba, next_rgba, duration, false)
            .await
    }

    /// Slide transition. `leftward = true`: current exits left, next enters
    /// from the right. `leftward = false`: mirrored.
    ///
    /// Async for the same reason as `show_fade`: the frame-budget sleep must
    /// yield instead of blocking the single-threaded executor.
    async fn show_slide(
        &mut self,
        current_rgba: Option<&RgbaImage>,
        next_rgba: &RgbaImage,
        duration: Duration,
        leftward: bool,
    ) -> Result<()> {
        if duration.is_zero() {
            return self.show_cut(next_rgba);
        }

        let tc = self.canvas.texture_creator();
        let w = self.width as i32;

        // Pre-bake textures once before the animation loop.
        let cur_tex = current_rgba
            .map(|cur| rgba_to_texture(&tc, cur))
            .transpose()?;
        let next_tex = rgba_to_texture(&tc, next_rgba)?;
        // Per-texture dimensions so each frame is drawn at the image's own
        // (aspect-correct) size, centred — never stretched to fill the screen
        // (which distorted letterboxed images mid-transition).
        let cur_dims = current_rgba.map(|r| (r.width(), r.height()));
        let (nw, nh) = (next_rgba.width(), next_rgba.height());
        let budget = Duration::from_millis(1000 / self.config.fps.max(1) as u64);
        let inv_dur = 1.0 / duration.as_secs_f32();
        self.canvas
            .set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= duration {
                break;
            }

            let t = elapsed.as_secs_f32() * inv_dur;
            // ease-in-out cubic
            let t = if t < 0.5 {
                4.0 * t * t * t
            } else {
                1.0 - (-2.0 * t + 2.0_f32).powi(3) / 2.0
            };
            let offset = (w as f32 * t) as i32;
            // Horizontal slide delta applied to each image's *centred* position:
            // current exits one edge while next enters from the opposite edge a
            // screen-width away. Vertical centring is fixed (no vertical motion).
            let (cur_dx, next_dx) = if leftward {
                (-offset, w - offset)
            } else {
                (offset, offset - w)
            };

            self.canvas.clear();

            if let (Some(ref cur), Some((cw, ch))) = (&cur_tex, cur_dims) {
                let cx = (self.width as i32 - cw as i32) / 2 + cur_dx;
                let cy = (self.height as i32 - ch as i32) / 2;
                self.canvas.copy(cur, None, Rect::new(cx, cy, cw, ch)).ok();
            }
            let nx = (self.width as i32 - nw as i32) / 2 + next_dx;
            let ny = (self.height as i32 - nh as i32) / 2;
            self.canvas
                .copy(&next_tex, None, Rect::new(nx, ny, nw, nh))
                .ok();
            self.canvas.present();

            let frame_time = start.elapsed() - elapsed;
            if frame_time < budget {
                tokio::time::sleep(budget - frame_time).await;
            }
        }

        // Final frame: reuse the pre-baked texture instead of re-allocating.
        self.canvas.clear();
        blit_centered(
            &mut self.canvas,
            &next_tex,
            next_rgba.width(),
            next_rgba.height(),
            self.width,
            self.height,
        )?;
        self.canvas.present();
        Ok(())
    }

    // ── Ken Burns ─────────────────────────────────────────────────────────────

    /// Render one Ken Burns frame: the image slowly zooms in (1.0 → 1.12)
    /// while drifting toward a corner chosen by `variant` (0–3).
    ///
    /// `t` is slide progress in [0, 1]. At t=0 the frame matches a plain
    /// `show_cut`, so the hand-off from any transition is seamless.
    ///
    /// Cost model: the texture is re-uploaded every call (~8 MB memcpy at
    /// 1080p). At 10–15 fps that is a few percent of one Zero 2 core — the
    /// price of not caching SDL textures across frames (their lifetimes are
    /// tied to the canvas). Feature is opt-in for exactly this reason.
    pub fn kb_frame(&mut self, rgba: &RgbaImage, t: f32, variant: u8) -> Result<()> {
        const MAX_ZOOM: f32 = 0.12;

        let t = t.clamp(0.0, 1.0);
        let zoom = 1.0 + MAX_ZOOM * t;

        let (iw, ih) = (rgba.width() as f32, rgba.height() as f32);
        let dw = (iw * zoom) as u32;
        let dh = (ih * zoom) as u32;

        // Centre position, then drift toward a corner by half the slack.
        let slack_x = (dw as i32 - self.width as i32).max(0) as f32;
        let slack_y = (dh as i32 - self.height as i32).max(0) as f32;
        let (dir_x, dir_y) = match variant % 4 {
            0 => (-0.5, -0.5),
            1 => (0.5, -0.5),
            2 => (-0.5, 0.5),
            _ => (0.5, 0.5),
        };
        let x = (self.width as i32 - dw as i32) / 2 + (dir_x * slack_x * t) as i32;
        let y = (self.height as i32 - dh as i32) / 2 + (dir_y * slack_y * t) as i32;

        let tc = self.canvas.texture_creator();
        let tex = rgba_to_texture(&tc, rgba)?;
        self.canvas
            .set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        self.canvas.clear();
        self.canvas
            .copy(&tex, None, Rect::new(x, y, dw, dh))
            .map_err(|e| anyhow::anyhow!("canvas copy (ken burns): {}", e))?;
        self.canvas.present();
        Ok(())
    }

    // ── Event loop ────────────────────────────────────────────────────────────

    /// Drain all pending SDL events into slideshow commands.
    ///
    /// Input is interpreted differently depending on whether the settings menu
    /// is open, so the caller passes the current `menu_open` state:
    ///
    /// - **Menu closed**: left/middle click → Prev/Next by screen half (matches
    ///   the on-screen ◄/► arrows); **right click → open the menu**; arrow keys
    ///   / space / P / F as before; `M` also opens the menu.
    /// - **Menu open**: right click or Esc → close; left click → activate the
    ///   row under the cursor (`MenuClick` carries the y); mouse motion →
    ///   `MenuPoint` (hover highlight); up/down → move; Enter/Space → activate.
    ///
    /// Returns every command in order (a `Vec`, not a single `Option`) so a
    /// burst of events in one poll is never dropped. Mouse-motion events are
    /// only emitted while the menu is open, so an idle slideshow produces none.
    pub fn poll_events(&mut self, menu_open: bool, editing: bool) -> Vec<SlideshowCmd> {
        // Toggle SDL text input to match the edit state. Doing it here (one
        // place, every tick) keeps it in lock-step with the menu without the
        // caller driving start/stop.
        if editing && !self.text_input_active {
            self.text_input.start();
            self.text_input_active = true;
        } else if !editing && self.text_input_active {
            self.text_input.stop();
            self.text_input_active = false;
        }

        let mut out = Vec::new();
        let half_w = self.width as i32 / 2;
        for event in self.event_pump.poll_iter() {
            // ── Text edit mode ─────────────────────────────────────────────
            // While a field is being edited, typed characters arrive as
            // TextInput events; Backspace/Enter/Esc are the control keys.
            // Everything else (menu nav, shortcuts, mouse) is ignored so it
            // can't fire mid-edit.
            if editing {
                match event {
                    Event::Quit { .. } => out.push(SlideshowCmd::Quit),
                    Event::TextInput { text, .. } => {
                        for ch in text.chars() {
                            out.push(SlideshowCmd::TextChar(ch));
                        }
                    }
                    Event::KeyDown {
                        keycode: Some(key), ..
                    } => match key {
                        Keycode::Backspace => out.push(SlideshowCmd::TextBackspace),
                        Keycode::Return | Keycode::KpEnter => out.push(SlideshowCmd::TextCommit),
                        Keycode::Escape => out.push(SlideshowCmd::TextCancel),
                        _ => {}
                    },
                    _ => {}
                }
                continue;
            }

            match event {
                // Window close always quits, menu or not.
                Event::Quit { .. } => out.push(SlideshowCmd::Quit),

                Event::KeyDown {
                    keycode: Some(key), ..
                } => {
                    if menu_open {
                        match key {
                            Keycode::Escape => out.push(SlideshowCmd::CloseMenu),
                            Keycode::Up => out.push(SlideshowCmd::MenuMove(-1)),
                            Keycode::Down => out.push(SlideshowCmd::MenuMove(1)),
                            Keycode::Return | Keycode::KpEnter | Keycode::Space => {
                                out.push(SlideshowCmd::MenuActivate)
                            }
                            _ => {}
                        }
                    } else {
                        match key {
                            Keycode::Escape | Keycode::Q => out.push(SlideshowCmd::Quit),
                            Keycode::Right | Keycode::Space => out.push(SlideshowCmd::Next),
                            Keycode::Left => out.push(SlideshowCmd::Prev),
                            Keycode::P => out.push(SlideshowCmd::TogglePause),
                            Keycode::F => out.push(SlideshowCmd::ToggleFavorite),
                            Keycode::M => out.push(SlideshowCmd::OpenMenu),
                            _ => {}
                        }
                    }
                }

                Event::MouseButtonDown {
                    mouse_btn, x, y, ..
                } => {
                    if menu_open {
                        match mouse_btn {
                            MouseButton::Right => out.push(SlideshowCmd::CloseMenu),
                            MouseButton::Left => out.push(SlideshowCmd::MenuClick { x, y }),
                            _ => {}
                        }
                    } else {
                        match mouse_btn {
                            // Right-click anywhere opens the settings menu.
                            MouseButton::Right => out.push(SlideshowCmd::OpenMenu),
                            // Left/middle: position-based prev/next.
                            _ => out.push(if x < half_w {
                                SlideshowCmd::Prev
                            } else {
                                SlideshowCmd::Next
                            }),
                        }
                    }
                }

                // Hover only matters while the menu is up; ignored otherwise so
                // an idle slideshow never wakes on mouse movement.
                Event::MouseMotion { x, y, .. } if menu_open => {
                    out.push(SlideshowCmd::MenuPoint { x, y })
                }

                _ => {}
            }
        }
        out
    }

    pub fn show_osd(&mut self, lines: &[String]) {
        for l in lines {
            info!("OSD: {}", l);
        }
        self.canvas
            .set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        self.canvas.clear();
        self.canvas.present();
    }
}

// ── Letterbox blur ────────────────────────────────────────────────────────────

/// Resize `src` to exactly `dw×dh` (stretch, no aspect preservation).
///
/// Cost note: clones the entire source pixel buffer (fir wants an owned
/// Vec). Only the letterbox-blur path calls this — once per slide on the
/// display-sized photo and once on a ~32-px thumbnail — so the copy is
/// cheap relative to the decode. Do NOT put this on a per-frame hot path
/// without switching to fir's borrowed-view API.
fn resize_rgba(
    src: &RgbaImage,
    dw: u32,
    dh: u32,
    alg: fast_image_resize::ResizeAlg,
) -> Option<RgbaImage> {
    use fast_image_resize as fir;
    let sw = std::num::NonZeroU32::new(src.width())?;
    let sh = std::num::NonZeroU32::new(src.height())?;
    let dw_nz = std::num::NonZeroU32::new(dw)?;
    let dh_nz = std::num::NonZeroU32::new(dh)?;

    let fir_src =
        fir::Image::from_vec_u8(sw, sh, src.as_raw().clone(), fir::PixelType::U8x4).ok()?;
    let mut dst = fir::Image::new(dw_nz, dh_nz, fir::PixelType::U8x4);
    let mut resizer = fir::Resizer::new(alg);
    resizer.resize(&fir_src.view(), &mut dst.view_mut()).ok()?;
    RgbaImage::from_raw(dw, dh, dst.into_vec())
}

/// Built-in megapixel backstop applied when `max_megapixels` is unset (0).
/// zune-jpeg decodes the full-resolution RGB before downscaling, so peak RAM
/// is roughly 3 MB per megapixel — this caps it (~72 MB at 24 MP) so an
/// oversized photo can't OOM a 512 MB Pi Zero 2. Override per install by
/// setting `max_megapixels` in the config.
const DEFAULT_MAX_MEGAPIXELS: u32 = 24;

/// Effective decode cap in megapixels: the configured value, or the built-in
/// backstop when unset (0).
fn megapixel_cap(configured: u32) -> u32 {
    if configured > 0 {
        configured
    } else {
        DEFAULT_MAX_MEGAPIXELS
    }
}

/// Reject a `w×h` image whose decoded pixel count exceeds the effective cap.
/// Checked from header dimensions *before* the full decode, so an oversized
/// photo is skipped without ever allocating its full-resolution buffer.
fn check_megapixels(configured: u32, w: u32, h: u32) -> Result<()> {
    let cap = megapixel_cap(configured);
    let actual = w as u64 * h as u64;
    if actual > cap as u64 * 1_000_000 {
        return Err(anyhow::anyhow!(
            "image {w}×{h} ({} MP) exceeds the {cap} MP limit — skipping \
             (raise max_megapixels or resize the photo)",
            actual / 1_000_000,
        ));
    }
    Ok(())
}

/// Nearest-neighbour downsample of `src` into a fresh `dw×dh` RGBA thumbnail.
///
/// Reads sampled pixels directly from `src` with no full-buffer clone (unlike
/// `resize_rgba`, which must hand `fir` an owned copy). Used only to build the
/// tiny blur source for the letterbox background, where nearest-neighbour
/// artefacts vanish under the box blur — so it touches ~`dw×dh` pixels
/// (e.g. 32×18 ≈ 576) instead of scanning the whole ~2 MP photo each slide.
fn downsample_nn(src: &RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    let (sw, sh) = src.dimensions();
    let (dw, dh) = (dw.max(1), dh.max(1));
    let mut out = RgbaImage::new(dw, dh);
    if sw == 0 || sh == 0 {
        return out;
    }
    let sbuf = src.as_raw();
    let obuf = out.as_mut();
    let sstride = sw as usize * 4;
    let ostride = dw as usize * 4;

    // Precompute the source byte-offset for each destination column once, so the
    // per-pixel inner loop is just indexed copies — no divides (ARM11 has no
    // hardware integer divider).
    let sx_lut: Vec<usize> = (0..dw as usize)
        .map(|ox| (ox as u64 * sw as u64 / dw as u64) as usize * 4)
        .collect();

    for oy in 0..dh as usize {
        let sy = (oy as u64 * sh as u64 / dh as u64) as usize;
        let srow = sy * sstride;
        let orow = oy * ostride;
        for (ox, &sxb) in sx_lut.iter().enumerate() {
            let s = srow + sxb;
            let o = orow + ox * 4;
            obuf[o..o + 4].copy_from_slice(&sbuf[s..s + 4]);
        }
    }
    out
}

/// Build a full-screen frame: heavily blurred, darkened, stretched copy of
/// `photo` as background, with the photo itself composited centred on top.
fn compose_letterbox_blur(photo: &RgbaImage, sw: u32, sh: u32) -> RgbaImage {
    use fast_image_resize::{FilterType, ResizeAlg};

    // 1. Thumbnail at screen aspect (stretch — invisible after the blur).
    //    Nearest-neighbour sampled straight from `photo`: reads only ~64×tiny_h
    //    pixels instead of cloning and scanning the whole display-sized buffer,
    //    and the sampling noise is washed out by the stack blur below.
    let tiny_w = 64u32;
    let tiny_h = ((tiny_w as u64 * sh as u64 / sw.max(1) as u64) as u32).max(2);
    let mut tiny = downsample_nn(photo, tiny_w, tiny_h);

    // 2. Blur using stackblur-iter and darken so the photo pops.
    //    Convert RGBA to u32 ARGB (stackblur-iter expects 0xAARRGGBB format).
    let mut pixels_u32: Vec<u32> = tiny
        .pixels()
        .map(|p| {
            let [r, g, b, a] = p.0;
            ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        })
        .collect();

    use stackblur_iter::imgref::ImgRefMut;
    let mut imgref = ImgRefMut::new(&mut pixels_u32, tiny_w as usize, tiny_h as usize);
    stackblur_iter::blur_argb(&mut imgref, 10);

    // Convert back to tiny RgbaImage and apply darkening.
    for (i, p) in tiny.pixels_mut().enumerate() {
        let val = pixels_u32[i];
        let a = ((val >> 24) & 0xff) as u8;
        let r = ((((val >> 16) & 0xff) as u32 * 11) >> 4) as u8; // × ~0.69
        let g = ((((val >> 8) & 0xff) as u32 * 11) >> 4) as u8;
        let b = (((val & 0xff) as u32 * 11) >> 4) as u8;
        *p = image::Rgba([r, g, b, a]);
    }

    // 3. Upscale to full screen (bilinear — cheap, mush is the goal).
    let Some(mut canvas) = resize_rgba(&tiny, sw, sh, ResizeAlg::Convolution(FilterType::Bilinear))
    else {
        return center_on_black(photo, sw, sh);
    };

    // 4. Composite the photo centred, row by row (straight memcpy).
    blit_into(&mut canvas, photo);
    canvas
}

/// Fallback compositor: photo centred on black.
fn center_on_black(photo: &RgbaImage, sw: u32, sh: u32) -> RgbaImage {
    let mut canvas = RgbaImage::from_pixel(sw, sh, image::Rgba([0, 0, 0, 255]));
    blit_into(&mut canvas, photo);
    canvas
}

/// Copy `photo` into the centre of `canvas` (clipped if larger).
fn blit_into(canvas: &mut RgbaImage, photo: &RgbaImage) {
    let (cw, ch) = canvas.dimensions();
    let (pw, ph) = photo.dimensions();
    let copy_w = pw.min(cw) as usize;
    let copy_h = ph.min(ch);
    let ox = (cw.saturating_sub(pw) / 2) as usize;
    let oy = ch.saturating_sub(ph) / 2;

    let canvas_stride = cw as usize * 4;
    let photo_stride = pw as usize * 4;
    let dst = canvas.as_mut();
    let src = photo.as_raw();
    for y in 0..copy_h as usize {
        let d = (oy as usize + y) * canvas_stride + ox * 4;
        let s = y * photo_stride;
        dst[d..d + copy_w * 4].copy_from_slice(&src[s..s + copy_w * 4]);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rgba_to_texture<'tc>(
    tc: &'tc TextureCreator<WindowContext>,
    rgba: &RgbaImage,
) -> Result<Texture<'tc>> {
    let (w, h) = (rgba.width(), rgba.height());
    let mut tex = tc
        .create_texture_streaming(PixelFormatEnum::RGBA32, w, h)
        .context("creating texture")?;
    let raw = rgba.as_raw();
    let row_bytes = w as usize * 4;
    tex.with_lock(None, |buf: &mut [u8], pitch: usize| {
        if pitch == row_bytes {
            buf[..raw.len()].copy_from_slice(raw);
        } else {
            for y in 0..h as usize {
                let src = y * row_bytes;
                let dst = y * pitch;
                buf[dst..dst + row_bytes].copy_from_slice(&raw[src..src + row_bytes]);
            }
        }
    })
    .map_err(|e| anyhow::anyhow!("texture lock: {}", e))?;
    Ok(tex)
}

fn blit_centered(
    canvas: &mut Canvas<Window>,
    tex: &Texture,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Result<()> {
    let x = ((screen_w as i32) - (img_w as i32)) / 2;
    let y = ((screen_h as i32) - (img_h as i32)) / 2;
    canvas
        .copy(tex, None, Rect::new(x, y, img_w, img_h))
        .map_err(|e| anyhow::anyhow!("canvas copy: {}", e))
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlideshowCmd {
    Next,
    Prev,
    TogglePause,
    ToggleFavorite,
    Quit,
    /// Open the settings menu (right-click / `M`).
    OpenMenu,
    /// Close the settings menu without selecting (right-click / Esc / click-away).
    CloseMenu,
    /// Move the menu selection by a relative amount (keyboard up/down).
    MenuMove(i32),
    /// Hover the menu row at this screen position (mouse motion).
    MenuPoint {
        x: i32,
        y: i32,
    },
    /// Click the menu row at this screen position (left button).
    MenuClick {
        x: i32,
        y: i32,
    },
    /// Activate the currently-selected menu row (Enter/Space).
    MenuActivate,
    /// A character typed while editing a menu text field.
    TextChar(char),
    /// Backspace while editing a menu text field.
    TextBackspace,
    /// Commit the edited field (Enter).
    TextCommit,
    /// Discard the edit (Esc).
    TextCancel,
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn megapixel_backstop_applies_when_unset() {
        // 0 → built-in backstop; configured value overrides it.
        assert_eq!(megapixel_cap(0), DEFAULT_MAX_MEGAPIXELS);
        assert_eq!(megapixel_cap(12), 12);

        // With no configured limit, a 48 MP photo is rejected by the backstop
        // but a 12 MP photo passes.
        assert!(check_megapixels(0, 8000, 6000).is_err()); // 48 MP
        assert!(check_megapixels(0, 4000, 3000).is_ok()); // 12 MP

        // A higher explicit limit lets the big photo through.
        assert!(check_megapixels(50, 8000, 6000).is_ok());
    }

    #[test]
    fn downsample_nn_produces_requested_dimensions() {
        let src = RgbaImage::from_pixel(100, 50, Rgba([10, 20, 30, 255]));
        let out = downsample_nn(&src, 32, 16);
        assert_eq!(out.dimensions(), (32, 16));
        // Uniform source → every sampled pixel is the source colour.
        assert_eq!(out.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(out.get_pixel(31, 15).0, [10, 20, 30, 255]);
    }

    #[test]
    fn downsample_nn_clamps_zero_dims_to_one() {
        let src = RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]));
        let out = downsample_nn(&src, 0, 0);
        assert_eq!(out.dimensions(), (1, 1));
    }

    #[test]
    fn downsample_nn_samples_correct_horizontal_region() {
        // Left half red, right half blue; the downsample must keep the split.
        let mut src = RgbaImage::new(100, 1);
        for x in 0..100u32 {
            let c = if x < 50 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            };
            src.put_pixel(x, 0, c);
        }
        let out = downsample_nn(&src, 10, 1);
        assert_eq!(out.get_pixel(0, 0).0, [255, 0, 0, 255]); // left → red
        assert_eq!(out.get_pixel(9, 0).0, [0, 0, 255, 255]); // right → blue
    }
}
