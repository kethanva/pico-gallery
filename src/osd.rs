/// On-screen display (OSD) overlay.
///
/// Renders a subtle info pill in the bottom-left corner of a photo showing
/// the album, capture date, and filename.  Uses a built-in 8×8 bitmap font
/// (font8x8 crate) so there are no system font dependencies — works headless
/// on a Pi Zero with no desktop environment.
///
/// # Pi Zero performance notes
///
/// All pixel writes go through the raw `RgbaImage::as_mut()` byte slice with
/// a precomputed row stride — no `get_pixel`/`put_pixel` per-pixel bounds
/// checks.  Darkening uses `(c * 7) >> 4` instead of `(c * 45) / 100`,
/// because ARM11 (Pi Zero / Pi Zero W) has no hardware integer divider.
/// On a 1080p screen this brings OSD render time from ~80 ms to ~5 ms.
use font8x8::UnicodeFonts;
use image::{Rgba, RgbaImage};
use picogallery_core::PhotoMeta;
use std::borrow::Cow;

const SCALE: u32 = 2;
const GLYPH_W: u32 = 8 * SCALE; // rendered character width  (16 px)
const GLYPH_H: u32 = 8 * SCALE; // rendered character height (16 px)
const LINE_GAP: u32 = 4; // vertical gap between text lines
const PAD: u32 = 8; // inset of text inside the dark background
const EDGE: u32 = 12; // distance of the pill from the screen edge
const MAX_LINE_CHARS: usize = 48;

// Nav arrow dimensions
const ARROW_W: i32 = 14; // triangle width in pixels
const ARROW_H: i32 = 22; // triangle height in pixels
const ARROW_PAD: u32 = 7; // padding inside the pill

const FG: [u8; 4] = [255, 255, 255, 255]; // foreground (white)
const SHADOW: [u8; 4] = [0, 0, 0, 255]; // 1-px drop shadow (black)

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncate `s` to at most `max` chars (adding "..." when cut).
/// Returns a borrow when no truncation is needed — saves an alloc per line
/// for the common case (short filenames, dates, album names).
fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.chars().count() <= max {
        Cow::Borrowed(s)
    } else {
        let cut: String = s.chars().take(max - 3).collect();
        Cow::Owned(format!("{}...", cut))
    }
}

/// Darken a rectangular region of `img` by ~44 % (`out = (in * 7) >> 4`).
///
/// Pure shift-and-multiply — no division (ARM11 has no hardware divider).
/// Operates on `img.as_mut()` directly so the inner loop has no per-pixel
/// bounds checks or index recomputation.
fn darken_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32) {
    let (iw, ih) = img.dimensions();
    let x_end = (x + w).min(iw);
    let y_end = (y + h).min(ih);
    if x >= x_end || y >= y_end {
        return;
    }

    let stride = iw as usize * 4;
    let x_start_bytes = x as usize * 4;
    let x_end_bytes = x_end as usize * 4;
    let buf = img.as_mut();

    for py in y..y_end {
        let row_off = py as usize * stride;
        let row = &mut buf[row_off + x_start_bytes..row_off + x_end_bytes];
        for chunk in row.chunks_exact_mut(4) {
            // RGB → multiply by 7 then shift right 4 (≈ × 0.4375).
            chunk[0] = ((chunk[0] as u32 * 7) >> 4) as u8;
            chunk[1] = ((chunk[1] as u32 * 7) >> 4) as u8;
            chunk[2] = ((chunk[2] as u32 * 7) >> 4) as u8;
            // chunk[3] (alpha) preserved — keep image opaque.
        }
    }
}

/// Draw a single 8×8 glyph scaled by `SCALE`× at `(ox, oy)`.
///
/// Two paths:
/// - **Fast path** — when the glyph is fully on-screen (the common case for
///   OSD text that we already truncate to fit), the inner loop writes
///   directly to the raw buffer with no per-pixel bounds checks.
/// - **Slow path** — per-pixel clipping for partial overlap, only at extreme
///   screen edges.
fn draw_glyph(img: &mut RgbaImage, rows: [u8; 8], ox: i32, oy: i32, color: [u8; 4]) {
    let (iw, ih) = img.dimensions();
    let gw = (8 * SCALE) as i32;
    let gh = (8 * SCALE) as i32;

    // Reject completely off-screen glyphs before touching the buffer.
    if ox + gw <= 0 || oy + gh <= 0 || ox >= iw as i32 || oy >= ih as i32 {
        return;
    }

    let stride = iw as usize * 4;
    let buf = img.as_mut();
    let [cr, cg, cb, ca] = color;

    let fully_visible = ox >= 0 && oy >= 0 && ox + gw <= iw as i32 && oy + gh <= ih as i32;

    if fully_visible {
        // ── Fast path: no per-pixel clip ──────────────────────────────────────
        let ox_u = ox as usize;
        let oy_u = oy as usize;
        for (row_i, &byte) in rows.iter().enumerate() {
            for col in 0..8usize {
                // font8x8: bit `col` (LSB-first) = column `col` in the glyph.
                if byte & (1 << col) != 0 {
                    let px_base = ox_u + col * SCALE as usize;
                    let py_base = oy_u + row_i * SCALE as usize;
                    for dy in 0..SCALE as usize {
                        let row_off = (py_base + dy) * stride;
                        for dx in 0..SCALE as usize {
                            let i = row_off + (px_base + dx) * 4;
                            buf[i] = cr;
                            buf[i + 1] = cg;
                            buf[i + 2] = cb;
                            buf[i + 3] = ca;
                        }
                    }
                }
            }
        }
    } else {
        // ── Slow path: per-pixel clip ─────────────────────────────────────────
        for (row_i, &byte) in rows.iter().enumerate() {
            for col in 0..8u32 {
                if byte & (1 << col) != 0 {
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            let px = ox + (col * SCALE + dx) as i32;
                            let py = oy + (row_i as u32 * SCALE + dy) as i32;
                            if px >= 0 && py >= 0 && (px as u32) < iw && (py as u32) < ih {
                                let i = (py as usize) * stride + (px as usize) * 4;
                                buf[i] = cr;
                                buf[i + 1] = cg;
                                buf[i + 2] = cb;
                                buf[i + 3] = ca;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn draw_char(img: &mut RgbaImage, ch: char, x: i32, y: i32) {
    let rows = font8x8::BASIC_FONTS
        .get(ch)
        .or_else(|| font8x8::BASIC_FONTS.get('?'))
        .unwrap_or([0u8; 8]);
    draw_glyph(img, rows, x + 1, y + 1, SHADOW);
    draw_glyph(img, rows, x, y, FG);
}

fn draw_text(img: &mut RgbaImage, text: &str, x: i32, y: i32) {
    for (i, ch) in text.chars().enumerate() {
        draw_char(img, ch, x + i as i32 * GLYPH_W as i32, y);
    }
}

// Marker used to keep the previous color-tuple constants out of the
// optimised path — they were only useful for the old `Rgba` API.
#[allow(dead_code)]
const _BG_HINT: Rgba<u8> = Rgba([0, 0, 0, 120]);

// ── Nav arrow helpers ─────────────────────────────────────────────────────────

/// Fill a left-pointing triangle (tip at left, base on right) at `(ox, oy)`.
/// Uses `ARROW_W × ARROW_H` dimensions.  Shadow/foreground drawn by callers.
fn fill_triangle_left(img: &mut RgbaImage, ox: i32, oy: i32, color: [u8; 4]) {
    let (iw, ih) = img.dimensions();
    let stride = iw as usize * 4;
    let buf = img.as_mut();
    let [cr, cg, cb, _] = color;
    let half = (ARROW_H - 1) as f32 / 2.0;
    for dy in 0..ARROW_H {
        // dist ∈ [0, 1]: 0 at the middle row (tip), 1 at top/bottom (base edge).
        let dist = ((dy as f32 - half) / half).abs();
        // xl: left edge of filled span; moves right as dist grows.
        let xl = (dist * (ARROW_W - 1) as f32 + 0.5) as i32;
        let py = oy + dy;
        if py < 0 || py >= ih as i32 {
            continue;
        }
        for dx in xl..ARROW_W {
            let px = ox + dx;
            if px < 0 || px >= iw as i32 {
                continue;
            }
            let i = py as usize * stride + px as usize * 4;
            buf[i] = cr;
            buf[i + 1] = cg;
            buf[i + 2] = cb;
            buf[i + 3] = 255;
        }
    }
}

/// Fill a right-pointing triangle (tip at right, base on left) at `(ox, oy)`.
fn fill_triangle_right(img: &mut RgbaImage, ox: i32, oy: i32, color: [u8; 4]) {
    let (iw, ih) = img.dimensions();
    let stride = iw as usize * 4;
    let buf = img.as_mut();
    let [cr, cg, cb, _] = color;
    let half = (ARROW_H - 1) as f32 / 2.0;
    for dy in 0..ARROW_H {
        let dist = ((dy as f32 - half) / half).abs();
        // xr: right edge of filled span (exclusive); shrinks as dist grows.
        let xr = ((1.0 - dist) * (ARROW_W - 1) as f32 + 0.5) as i32 + 1;
        let py = oy + dy;
        if py < 0 || py >= ih as i32 {
            continue;
        }
        for dx in 0..xr {
            let px = ox + dx;
            if px < 0 || px >= iw as i32 {
                continue;
            }
            let i = py as usize * stride + px as usize * 4;
            buf[i] = cr;
            buf[i + 1] = cg;
            buf[i + 2] = cb;
            buf[i + 3] = 255;
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Maximum number of text lines in the info pill — keeps it from growing tall
/// enough to cover the photo when a source supplies lots of metadata.
const MAX_LINES: usize = 4;

/// Stamp a photo-info pill in the bottom-left corner of `img`.
///
/// Lines displayed (when present), top to bottom, capped at `MAX_LINES`:
///   1. Title      (`meta.extra["title"]`)      — richer sources only
///   2. Album name (`meta.extra["album"]`)      — skipped if equal to title
///   3. Location   (`meta.extra["location"]`)   — "City, Country"
///   4. Capture date (from `exif_date` or `meta.taken_at`)
///   5. Filename   (`meta.filename`)            — only when no title is present
///
/// Sources without the richer keys (plain directories, etc.) fall back to the
/// original album / date / filename pill. Does nothing when there is no text.
pub fn draw_photo_info(img: &mut RgbaImage, meta: &PhotoMeta, exif_date: Option<&str>) {
    // `taken_at` fallback owns its String; keep it alive for the borrows below.
    let taken_at_str: Option<String> = meta.taken_at.map(|dt| dt.format("%Y-%m-%d").to_string());

    let nonempty = |k: &str| meta.extra.get(k).filter(|s| !s.is_empty());
    let title = nonempty("title");

    let mut lines: Vec<Cow<'_, str>> = Vec::with_capacity(MAX_LINES);
    if let Some(t) = title {
        lines.push(truncate(t, MAX_LINE_CHARS));
    }
    if let Some(album) = nonempty("album") {
        // Avoid repeating the same text when title == album.
        if title.map(|t| t != album).unwrap_or(true) {
            lines.push(truncate(album, MAX_LINE_CHARS));
        }
    }
    if let Some(loc) = nonempty("location") {
        lines.push(truncate(loc, MAX_LINE_CHARS));
    }
    // Prefer EXIF date; fall back to meta.taken_at.
    if let Some(d) = exif_date.or(taken_at_str.as_deref()) {
        lines.push(truncate(d, MAX_LINE_CHARS));
    }
    // Filename is the identity fallback — only worth a line when there's no
    // title to name the photo.
    if title.is_none() && !meta.filename.is_empty() {
        lines.push(truncate(&meta.filename, MAX_LINE_CHARS));
    }
    lines.truncate(MAX_LINES);
    if lines.is_empty() {
        return;
    }

    let n = lines.len() as u32;
    let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let box_w = max_chars * GLYPH_W + PAD * 2;
    let box_h = n * GLYPH_H + (n - 1) * LINE_GAP + PAD * 2;

    let (_, ih) = img.dimensions();
    let bx = EDGE;
    let by = ih.saturating_sub(box_h + EDGE);

    darken_rect(img, bx, by, box_w, box_h);

    for (i, line) in lines.iter().enumerate() {
        let tx = (bx + PAD) as i32;
        let ty = (by + PAD + i as u32 * (GLYPH_H + LINE_GAP)) as i32;
        draw_text(img, line.as_ref(), tx, ty);
    }
}

/// Draw left (◄) and right (►) navigation arrow pills on the vertical
/// centre of the left and right screen edges.
///
/// Each arrow is a filled triangle inside a darkened pill, matching the
/// OSD style (white glyph + 1-px black drop-shadow).  The arrows hint to
/// users that left/right clicks and arrow keys advance or reverse the slideshow.
/// Centring keeps the left pill clear of the photo-info pill in the
/// bottom-left corner and matches the click hit-zones (full left/right
/// screen halves) handled by the renderer.
pub fn draw_nav_arrows(img: &mut RgbaImage) {
    let (iw, ih) = img.dimensions();
    let pill_w = ARROW_W as u32 + ARROW_PAD * 2;
    let pill_h = ARROW_H as u32 + ARROW_PAD * 2;
    let by = (ih / 2).saturating_sub(pill_h / 2);

    // ── Left arrow (◄) ───────────────────────────────────────────────────────
    let lx = EDGE;
    darken_rect(img, lx, by, pill_w, pill_h);
    let ax = lx as i32 + ARROW_PAD as i32;
    let ay = by as i32 + ARROW_PAD as i32;
    fill_triangle_left(img, ax + 1, ay + 1, SHADOW);
    fill_triangle_left(img, ax, ay, FG);

    // ── Right arrow (►) ──────────────────────────────────────────────────────
    let rx = iw.saturating_sub(pill_w + EDGE);
    darken_rect(img, rx, by, pill_w, pill_h);
    let bx = rx as i32 + ARROW_PAD as i32;
    fill_triangle_right(img, bx + 1, ay + 1, SHADOW);
    fill_triangle_right(img, bx, ay, FG);
}

// ── Favourite indicator ─────────────────────────────────────────────────────

const FAV_RED: [u8; 4] = [230, 60, 70, 255]; // heart fill colour

/// 8×8 heart bitmap, one byte per row, bit `col` (LSB-first) = column `col` —
/// the same layout `draw_glyph` expects for font8x8 glyphs.
const HEART: [u8; 8] = [
    0b0011_0110, // .XX..XX.
    0b1111_1111, // XXXXXXXX
    0b1111_1111, // XXXXXXXX
    0b1111_1111, // XXXXXXXX
    0b0111_1110, // .XXXXXX.
    0b0011_1100, // ..XXXX..
    0b0001_1000, // ...XX...
    0b0000_0000, // ........
];

/// Stamp a small red ♥ pill in the top-right corner, marking the on-screen
/// photo as a favourite. Same dark-pill + drop-shadow styling as the rest of
/// the OSD; one cheap glyph blit, no per-frame cost.
pub fn draw_favorite(img: &mut RgbaImage) {
    let (iw, _ih) = img.dimensions();
    let box_w = GLYPH_W + PAD * 2;
    let box_h = GLYPH_H + PAD * 2;
    let bx = iw.saturating_sub(box_w + EDGE);
    let by = EDGE;

    darken_rect(img, bx, by, box_w, box_h);
    let gx = (bx + PAD) as i32;
    let gy = (by + PAD) as i32;
    draw_glyph(img, HEART, gx + 1, gy + 1, SHADOW);
    draw_glyph(img, HEART, gx, gy, FAV_RED);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn white_img(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn draw_nav_arrows_does_not_panic_on_typical_resolution() {
        let mut img = white_img(1920, 1080);
        draw_nav_arrows(&mut img); // must not panic
    }

    #[test]
    fn draw_nav_arrows_does_not_panic_on_small_image() {
        let mut img = white_img(80, 80);
        draw_nav_arrows(&mut img);
    }

    #[test]
    fn draw_nav_arrows_does_not_panic_on_tiny_image() {
        let mut img = white_img(1, 1);
        draw_nav_arrows(&mut img);
    }

    #[test]
    fn left_arrow_pixels_darkened() {
        let (w, h) = (320u32, 240u32);
        let mut img = white_img(w, h);
        draw_nav_arrows(&mut img);
        // Pill on the left edge: x starts at EDGE, y centred on ih / 2.
        let pill_center_x = EDGE + ARROW_PAD / 2;
        let pill_center_y = h / 2;
        let px = img.get_pixel(pill_center_x, pill_center_y);
        // darken_rect multiplies by 7/16 ≈ 0.44 — white (255) → ≤ 112
        assert!(
            px[0] < 200,
            "expected darkened pill at left arrow, got {:?}",
            px
        );
    }

    #[test]
    fn draw_favorite_does_not_panic_and_marks_corner() {
        let (w, h) = (320u32, 240u32);
        let mut img = white_img(w, h);
        draw_favorite(&mut img);
        // Pill sits in the top-right; its centre should no longer be pure white.
        let box_w = GLYPH_W + PAD * 2;
        let cx = w - box_w / 2 - EDGE;
        let cy = EDGE + (GLYPH_H + PAD * 2) / 2;
        let px = img.get_pixel(cx, cy);
        assert!(
            px[0] < 250 || px[1] < 250 || px[2] < 250,
            "expected the favourite pill to alter the corner, got {:?}",
            px
        );
    }

    #[test]
    fn draw_favorite_does_not_panic_on_tiny_image() {
        let mut img = white_img(1, 1);
        draw_favorite(&mut img);
    }

    #[test]
    fn right_arrow_pixels_darkened() {
        let (w, h) = (320u32, 240u32);
        let mut img = white_img(w, h);
        draw_nav_arrows(&mut img);
        let pill_w = ARROW_W as u32 + ARROW_PAD * 2;
        let rx = w.saturating_sub(pill_w + EDGE);
        let pill_center_x = rx + ARROW_PAD / 2;
        let pill_center_y = h / 2;
        let px = img.get_pixel(pill_center_x, pill_center_y);
        assert!(
            px[0] < 200,
            "expected darkened pill at right arrow, got {:?}",
            px
        );
    }
}
