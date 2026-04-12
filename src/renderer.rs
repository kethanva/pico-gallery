/// Renderer — SDL2 with KMS/DRM backend (Linux) or native backend (macOS/dev).
///
/// On Linux: probes `/dev/dri/card*` via the `drm` crate at startup to find the
/// correct card and native resolution, then hands that to SDL2's kmsdrm driver.
/// On macOS/other: SDL2 uses its native backend (Cocoa/Metal) automatically.
use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};
use log::{debug, info, warn};
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

use crate::config::{DisplayConfig, Transition};

// ── DRM display probe (Linux only) ───────────────────────────────────────────

#[cfg(target_os = "linux")]
mod drm_probe {
    use drm::control::{connector, Device as ControlDevice};
    use drm::Device;
    use log::{info, warn};
    use std::os::unix::io::{AsRawFd, RawFd};

    pub struct DrmCard(pub std::fs::File);
    impl AsRawFd for DrmCard { fn as_raw_fd(&self) -> RawFd { self.0.as_raw_fd() } }
    impl Device for DrmCard {}
    impl ControlDevice for DrmCard {}

    /// Scan /dev/dri/card0..3 and return (device_path, width, height) for the
    /// first card that has a connected display. Returns None if nothing found.
    pub fn probe() -> Option<(String, u32, u32)> {
        for n in 0..4 {
            let path = format!("/dev/dri/card{}", n);
            let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).ok()?;
            let card = DrmCard(file);
            let res  = match card.resource_handles() { Ok(r) => r, Err(_) => continue };
            for &conn_h in res.connectors() {
                let info = match card.get_connector(conn_h, false) { Ok(i) => i, Err(_) => continue };
                if info.state() != connector::State::Connected { continue; }
                if let Some(mode) = info.modes().first() {
                    let (w, h) = (mode.size().0 as u32, mode.size().1 as u32);
                    info!("DRM probe: {} — {:?} connected, native {}×{}", path, info.interface(), w, h);
                    return Some((path, w, h));
                }
            }
        }
        warn!("DRM probe: no connected display found in /dev/dri/card0..3");
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
        // ── KMS/DRM backend (Linux only) ──────────────────────────────────────
        #[cfg(target_os = "linux")]
        {
            // Only force kmsdrm if the user hasn't overridden (e.g. SDL_VIDEODRIVER=x11 for dev).
            if std::env::var("SDL_VIDEODRIVER").is_err() {
                std::env::set_var("SDL_VIDEODRIVER", "kmsdrm");
            }
        }

        // ── DRM display probe (Linux only) ────────────────────────────────────
        // Finds the correct /dev/dri/cardN (Pi 4/5 has display on card1, not card0)
        // and the native resolution. On macOS this block is compiled away entirely.
        #[cfg(target_os = "linux")]
        let (probed_w, probed_h) = {
            if let Some((dev_path, w, h)) = drm_probe::probe() {
                if std::env::var("SDL_VIDEO_KMSDRM_DEVICE").is_err() {
                    std::env::set_var("SDL_VIDEO_KMSDRM_DEVICE", &dev_path);
                }
                (w, h)
            } else {
                (0u32, 0u32)
            }
        };
        #[cfg(not(target_os = "linux"))]
        let (probed_w, probed_h) = (0u32, 0u32);

        let sdl_ctx = sdl2::init().map_err(|e| anyhow::anyhow!("SDL init: {}", e))?;
        let video   = sdl_ctx.video().map_err(|e| anyhow::anyhow!("SDL video: {}", e))?;

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
        let img = image::load_from_memory(bytes).context("decoding image")?;
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
        let mut next_tex = rgba_to_texture(&tc, next_rgba)?;
        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= duration { break; }

            let alpha = ((elapsed.as_secs_f32() / duration.as_secs_f32()) * 255.0) as u8;
            self.canvas.clear();

            if let Some(cur) = current_rgba {
                let cur_tex = rgba_to_texture(&tc, cur)?;
                blit_centered(&mut self.canvas, &cur_tex, cur.width(), cur.height(), self.width, self.height)?;
            }
            next_tex.set_alpha_mod(alpha);
            self.canvas.set_blend_mode(sdl2::render::BlendMode::Blend);
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
