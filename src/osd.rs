/// On-screen display (OSD) overlay.
///
/// Renders a subtle info pill in the bottom-left corner of a photo showing
/// the album, capture date, and filename.  Uses a built-in 8×8 bitmap font
/// (font8x8 crate) so there are no system font dependencies — works headless
/// on a Pi Zero with no desktop environment.
///
/// Text is scaled 2× (16px tall), white with a 1px drop shadow, on a
/// darkened background for readability against any photo.
use font8x8::UnicodeFonts;
use image::{Rgba, RgbaImage};
use picogallery_core::PhotoMeta;

const SCALE: u32 = 2;
const GLYPH_W: u32 = 8 * SCALE; // rendered character width  (16 px)
const GLYPH_H: u32 = 8 * SCALE; // rendered character height (16 px)
const LINE_GAP: u32 = 4;        // vertical gap between text lines
const PAD: u32 = 8;             // inset of text inside the dark background
const EDGE: u32 = 12;           // distance of the pill from the screen edge
const MAX_LINE_CHARS: usize = 48;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max - 3).collect();
        format!("{}...", cut)
    }
}

fn draw_glyph(img: &mut RgbaImage, rows: [u8; 8], ox: i32, oy: i32, color: Rgba<u8>) {
    let (iw, ih) = img.dimensions();
    for (row_i, &byte) in rows.iter().enumerate() {
        for col in 0..8u32 {
            // font8x8: bit 0 (LSB) is the leftmost column of the glyph.
            if byte & (1 << col) != 0 {
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let px = ox + (col * SCALE + dx) as i32;
                        let py = oy + (row_i as u32 * SCALE + dy) as i32;
                        if px >= 0 && py >= 0 {
                            let (ux, uy) = (px as u32, py as u32);
                            if ux < iw && uy < ih {
                                img.put_pixel(ux, uy, color);
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
    draw_glyph(img, rows, x + 1, y + 1, Rgba([0, 0, 0, 255]));   // shadow
    draw_glyph(img, rows, x,     y,     Rgba([255, 255, 255, 255])); // foreground
}

fn draw_text(img: &mut RgbaImage, text: &str, x: i32, y: i32) {
    for (i, ch) in text.chars().enumerate() {
        draw_char(img, ch, x + i as i32 * GLYPH_W as i32, y);
    }
}

/// Darken a rectangular region to create a semi-opaque background.
fn darken_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32) {
    let (iw, ih) = img.dimensions();
    for py in y..(y + h).min(ih) {
        for px in x..(x + w).min(iw) {
            let Rgba([r, g, b, a]) = *img.get_pixel(px, py);
            // Blend toward black at ~55% opacity (multiply by 0.45).
            img.put_pixel(px, py, Rgba([
                (r as u32 * 45 / 100) as u8,
                (g as u32 * 45 / 100) as u8,
                (b as u32 * 45 / 100) as u8,
                a,
            ]));
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Stamp a photo-info pill in the bottom-left corner of `img`.
///
/// Lines displayed (when present), top to bottom:
///   1. Album name  (`meta.extra["album"]`)
///   2. Capture date (from `exif_date` or `meta.taken_at`)
///   3. Filename     (`meta.filename`)
///
/// Does nothing when there is no text to render.
pub fn draw_photo_info(img: &mut RgbaImage, meta: &PhotoMeta, exif_date: Option<&str>) {
    let date_str: Option<String> = exif_date
        .map(|s| s.to_owned())
        .or_else(|| meta.taken_at.map(|dt| dt.format("%Y-%m-%d").to_string()));

    let mut lines: Vec<String> = Vec::with_capacity(3);
    if let Some(album) = meta.extra.get("album") {
        if !album.is_empty() {
            lines.push(truncate(album, MAX_LINE_CHARS));
        }
    }
    if let Some(ref d) = date_str {
        lines.push(truncate(d, MAX_LINE_CHARS));
    }
    if !meta.filename.is_empty() {
        lines.push(truncate(&meta.filename, MAX_LINE_CHARS));
    }
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
        draw_text(img, line, tx, ty);
    }
}
