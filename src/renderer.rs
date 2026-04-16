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
    pixels::PixelFormatEnum,
    rect::Rect,
    render::{Canvas, Texture, TextureCreator},
    video::{Window, WindowContext},
    Sdl,
};
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
    impl AsFd for DrmCard { fn as_fd(&self) -> BorrowedFd<'_> { self.0.as_fd() } }
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
                warn!("DRM probe: cannot read /dev/dri — {} (is the kernel module loaded?)", e);
                return None;
            }
        }

        for n in 0..4 {
            let path = format!("/dev/dri/card{}", n);

            if !std::path::Path::new(&path).exists() {
                continue;
            }

            // Read-only is enough for enumeration; requires only `video` group.
            let file = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
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
    sdl_ctx: Sdl,
    canvas:  Canvas<Window>,
    width:   u32,
    height:  u32,
    config:  DisplayConfig,
}

impl Renderer {
    pub fn init(config: DisplayConfig) -> Result<Self> {
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
            let has_x11    = std::env::var("DISPLAY").is_ok();
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
                warn!("SDL was compiled with these video drivers: [{}]", drivers.join(", "));

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

        // ── Resolution: config > DRM probe > SDL2 desktop query ───────────────
        let (w, h) = if config.width > 0 && config.height > 0 {
            (config.width, config.height)
        } else if probed_w > 0 {
            (probed_w, probed_h)
        } else {
            let dm = video.desktop_display_mode(0)
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

        Ok(Self { sdl_ctx, canvas, width: w, height: h, config })
    }

    pub fn width(&self)  -> u32 { self.width  }
    pub fn height(&self) -> u32 { self.height }

    // ── Image decode & scale ─────────────────────────────────────────────────

    pub fn decode_and_scale(&self, bytes: &[u8]) -> Result<RgbaImage> {
        // Reject suspiciously large blobs before handing to the decoder.
        const MAX_DECODE_BYTES: usize = 50 * 1024 * 1024;
        if bytes.len() > MAX_DECODE_BYTES {
            return Err(anyhow::anyhow!(
                "image blob too large ({} MB) — refusing to decode",
                bytes.len() / 1_048_576
            ));
        }
        let img = image::load_from_memory(bytes).context("decoding image")?;
        // Reject extreme dimensions that could cause OOM during RGBA conversion.
        if img.width() > 16_000 || img.height() > 16_000 {
            return Err(anyhow::anyhow!(
                "image dimensions {}×{} exceed safety limit",
                img.width(), img.height()
            ));
        }
        Ok(self.scale_image(img).into_rgba8())
    }

    fn scale_image(&self, img: DynamicImage) -> DynamicImage {
        let (sw, sh) = (img.width(), img.height());
        let (dw, dh) = (self.width, self.height);

        let (nw, nh) = if self.config.fill_screen {
            let scale = f32::max(dw as f32 / sw as f32, dh as f32 / sh as f32);
            ((sw as f32 * scale) as u32, (sh as f32 * scale) as u32)
        } else {
            let scale = f32::min(dw as f32 / sw as f32, dh as f32 / sh as f32);
            ((sw as f32 * scale) as u32, (sh as f32 * scale) as u32)
        };

        use fast_image_resize as fir;
        let src = fir::Image::from_vec_u8(
            std::num::NonZeroU32::new(sw).unwrap(),
            std::num::NonZeroU32::new(sh).unwrap(),
            img.into_rgba8().into_raw(),
            fir::PixelType::U8x4,
        ).unwrap_or_else(|_| fir::Image::new(
            std::num::NonZeroU32::new(1).unwrap(),
            std::num::NonZeroU32::new(1).unwrap(),
            fir::PixelType::U8x4,
        ));

        let mut dst = fir::Image::new(
            std::num::NonZeroU32::new(nw.max(1)).unwrap(),
            std::num::NonZeroU32::new(nh.max(1)).unwrap(),
            fir::PixelType::U8x4,
        );

        let mut resizer = fir::Resizer::new(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3));
        if let Err(e) = resizer.resize(&src.view(), &mut dst.view_mut()) {
            warn!("Image resize error: {}", e);
            return DynamicImage::new_rgba8(self.width, self.height);
        }

        let raw = dst.into_vec();
        image::ImageBuffer::from_raw(nw, nh, raw)
            .map(DynamicImage::ImageRgba8)
            .unwrap_or_else(|| DynamicImage::new_rgba8(self.width, self.height))
    }

    // ── Display methods ──────────────────────────────────────────────────────

    pub fn show_cut(&mut self, rgba: &RgbaImage) -> Result<()> {
        let tc  = self.canvas.texture_creator();
        let tex = rgba_to_texture(&tc, rgba)?;
        self.canvas.clear();
        blit_centered(&mut self.canvas, &tex, rgba.width(), rgba.height(), self.width, self.height)?;
        self.canvas.present();
        Ok(())
    }

    pub fn show_fade(
        &mut self,
        current_rgba: Option<&RgbaImage>,
        next_rgba: &RgbaImage,
        duration: Duration,
    ) -> Result<()> {
        if duration.is_zero() { return self.show_cut(next_rgba); }

        let tc = self.canvas.texture_creator();
        // Pre-bake current frame texture (outside the loop — avoids re-allocating every frame).
        let cur_tex = match current_rgba {
            Some(cur) => Some(rgba_to_texture(&tc, cur)?),
            None      => None,
        };
        let mut next_tex = rgba_to_texture(&tc, next_rgba)?;
        // SDL_RenderCopy only respects set_alpha_mod when the texture blend mode is
        // SDL_BLENDMODE_BLEND.  Without this the alpha_mod writes transparent pixels
        // to the Metal framebuffer and macOS shows the white desktop behind the window.
        next_tex.set_blend_mode(sdl2::render::BlendMode::Blend);

        let start = Instant::now();
        loop {
            let elapsed = start.elapsed();
            if elapsed >= duration { break; }

            let alpha = ((elapsed.as_secs_f32() / duration.as_secs_f32()) * 255.0) as u8;
            self.canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
            self.canvas.clear();

            if let Some(ref cur) = cur_tex {
                blit_centered(&mut self.canvas, cur, current_rgba.unwrap().width(), current_rgba.unwrap().height(), self.width, self.height)?;
            }
            next_tex.set_alpha_mod(alpha);
            blit_centered(&mut self.canvas, &next_tex, next_rgba.width(), next_rgba.height(), self.width, self.height)?;
            self.canvas.present();

            let budget = Duration::from_millis(1000 / self.config.fps as u64);
            let used   = start.elapsed() - elapsed;
            if used < budget { std::thread::sleep(budget - used); }
        }
        self.show_cut(next_rgba)
    }

    pub fn show_slide_left(
        &mut self,
        current_rgba: Option<&RgbaImage>,
        next_rgba: &RgbaImage,
        duration: Duration,
    ) -> Result<()> {
        if duration.is_zero() { return self.show_cut(next_rgba); }

        let tc    = self.canvas.texture_creator();
        let w     = self.width as i32;
        let h     = self.height as i32;
        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= duration { break; }

            let t = elapsed.as_secs_f32() / duration.as_secs_f32();
            // ease-in-out cubic
            let t = if t < 0.5 { 4.0*t*t*t } else { 1.0 - (-2.0*t + 2.0_f32).powi(3) / 2.0 };
            let offset = (w as f32 * t) as i32;

            self.canvas.clear();
            self.canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
            self.canvas.fill_rect(Rect::new(0, 0, w as u32, h as u32)).ok();

            if let Some(cur) = current_rgba {
                let tex = rgba_to_texture(&tc, cur)?;
                self.canvas.copy(&tex, None, Rect::new(-offset, 0, self.width, self.height)).ok();
            }
            let next_tex = rgba_to_texture(&tc, next_rgba)?;
            self.canvas.copy(&next_tex, None, Rect::new(w - offset, 0, self.width, self.height)).ok();
            self.canvas.present();

            let budget = Duration::from_millis(1000 / self.config.fps as u64);
            let used   = start.elapsed() - elapsed;
            if used < budget { std::thread::sleep(budget - used); }
        }
        self.show_cut(next_rgba)
    }

    // ── Event loop ────────────────────────────────────────────────────────────

    pub fn poll_events(&self) -> Option<SlideshowCmd> {
        let mut ep = self.sdl_ctx.event_pump().ok()?;
        for event in ep.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown { keycode: Some(Keycode::Escape), .. }
                | Event::KeyDown { keycode: Some(Keycode::Q), .. } => return Some(SlideshowCmd::Quit),
                Event::KeyDown { keycode: Some(Keycode::Right), .. }
                | Event::KeyDown { keycode: Some(Keycode::Space), .. } => return Some(SlideshowCmd::Next),
                Event::KeyDown { keycode: Some(Keycode::Left), .. }    => return Some(SlideshowCmd::Prev),
                Event::KeyDown { keycode: Some(Keycode::P), .. }       => return Some(SlideshowCmd::TogglePause),
                Event::MouseButtonDown { mouse_btn: sdl2::mouse::MouseButton::Left, .. } => return Some(SlideshowCmd::Prev),
                Event::MouseButtonDown { mouse_btn: sdl2::mouse::MouseButton::Right, .. } => return Some(SlideshowCmd::Next),
                _ => {}
            }
        }
        None
    }

    pub fn show_osd(&mut self, lines: &[String]) {
        for l in lines { info!("OSD: {}", l); }
        self.canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        self.canvas.clear();
        self.canvas.present();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rgba_to_texture<'tc>(
    tc:   &'tc TextureCreator<WindowContext>,
    rgba: &RgbaImage,
) -> Result<Texture<'tc>> {
    let (w, h) = (rgba.width(), rgba.height());
    let mut tex = tc
        .create_texture_streaming(PixelFormatEnum::RGBA32, w, h)
        .context("creating texture")?;
    tex.with_lock(None, |buf: &mut [u8], pitch: usize| {
        for y in 0..h as usize {
            for x in 0..w as usize {
                let px  = rgba.get_pixel(x as u32, y as u32).0;
                let off = y * pitch + x * 4;
                buf[off]     = px[0];
                buf[off + 1] = px[1];
                buf[off + 2] = px[2];
                buf[off + 3] = px[3];
            }
        }
    }).map_err(|e| anyhow::anyhow!("texture lock: {}", e))?;
    Ok(tex)
}

fn blit_centered(
    canvas:   &mut Canvas<Window>,
    tex:      &Texture,
    img_w:    u32,
    img_h:    u32,
    screen_w: u32,
    screen_h: u32,
) -> Result<()> {
    let x = ((screen_w as i32) - (img_w as i32)) / 2;
    let y = ((screen_h as i32) - (img_h as i32)) / 2;
    canvas.copy(tex, None, Rect::new(x, y, img_w, img_h))
        .map_err(|e| anyhow::anyhow!("canvas copy: {}", e))
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlideshowCmd { Next, Prev, TogglePause, Quit }
