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

/// Draw `ch` in `color` with a 1-px black drop shadow.
fn draw_char_colored(img: &mut RgbaImage, ch: char, x: i32, y: i32, color: [u8; 4]) {
    let rows = font8x8::BASIC_FONTS
        .get(ch)
        .or_else(|| font8x8::BASIC_FONTS.get('?'))
        .unwrap_or([0u8; 8]);
    draw_glyph(img, rows, x + 1, y + 1, SHADOW);
    draw_glyph(img, rows, x, y, color);
}

/// Draw `text` in `color` starting at `(x, y)`.
fn draw_text_colored(img: &mut RgbaImage, text: &str, x: i32, y: i32, color: [u8; 4]) {
    for (i, ch) in text.chars().enumerate() {
        draw_char_colored(img, ch, x + i as i32 * GLYPH_W as i32, y, color);
    }
}

fn draw_text(img: &mut RgbaImage, text: &str, x: i32, y: i32) {
    draw_text_colored(img, text, x, y, FG);
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

// ── Clock ─────────────────────────────────────────────────────────────────────

/// Stamp an `HH:MM` clock pill centred along the top edge. Same dark-pill +
/// drop-shadow styling as the rest of the OSD; one cheap blit per slide, so it
/// adds no per-frame cost on a Pi Zero. Caller supplies the formatted string.
pub fn draw_clock(img: &mut RgbaImage, text: &str) {
    if text.is_empty() {
        return;
    }
    let (iw, _ih) = img.dimensions();
    let chars = text.chars().count() as u32;
    let box_w = chars * GLYPH_W + PAD * 2;
    let box_h = GLYPH_H + PAD * 2;
    let bx = iw.saturating_sub(box_w) / 2;
    let by = EDGE;

    darken_rect(img, bx, by, box_w, box_h);
    draw_text(img, text, (bx + PAD) as i32, (by + PAD) as i32);
}

// ── Settings menu ───────────────────────────────────────────────────────────────

const MENU_ROW_H: u32 = GLYPH_H + 10; // row pitch (text + breathing room)
const MENU_PAD: u32 = 20; // panel inner padding
const MENU_BG: [u8; 4] = [16, 18, 26, 232]; // translucent panel background
const MENU_SHADOW: [u8; 4] = [0, 0, 0, 120]; // soft drop shadow behind the panel
const MENU_BORDER: [u8; 4] = [78, 104, 168, 255]; // 2-px accent frame
const MENU_RULE: [u8; 4] = [120, 150, 215, 110]; // hairline under the title
const MENU_SEL: [u8; 4] = [46, 70, 122, 255]; // selected-row highlight
const MENU_SEL_BAR: [u8; 4] = [128, 178, 255, 255]; // accent bar on the selected row
const MENU_HEADER: [u8; 4] = [128, 156, 214, 255]; // section-header text (accent)
const MENU_LABEL: [u8; 4] = [232, 236, 244, 255]; // item label text
const MENU_VALUE: [u8; 4] = [150, 206, 198, 255]; // value text (after "label: ")
const SEL_BAR_W: u32 = 4; // width of the selected-row accent bar
const SHADOW_OFF: u32 = 8; // drop-shadow offset (px, down-right)

/// One rendered menu line for the drawing/hit-testing layer: the label text and
/// whether it is a non-selectable section header. Keeps `osd` decoupled from the
/// `menu` module's row model — the caller flattens its rows into these.
pub struct MenuItem<'a> {
    pub label: &'a str,
    pub is_header: bool,
}

/// Full panel geometry shared by `draw_menu` and `menu_hit_test` — the single
/// source of truth so a click maps to exactly the row it lands on, in both
/// axes. Rows are a uniform `MENU_ROW_H` tall (headers included) so the mapping
/// stays a simple division. Returns `(panel_x, panel_top, panel_w, panel_h,
/// rows_top, row_h)`. `panel_x`/`panel_top` may be negative only in degenerate
/// cases (panel wider or taller than the screen); callers clamp before drawing.
fn menu_geometry(
    img_w: u32,
    img_h: u32,
    title: &str,
    rows: &[MenuItem],
) -> (i32, i32, u32, u32, i32, u32) {
    // Width from the widest line (title or any row).
    let longest = rows
        .iter()
        .map(|r| r.label.chars().count())
        .chain(std::iter::once(title.chars().count()))
        .max()
        .unwrap_or(0) as u32;
    let panel_w = (longest * GLYPH_W + MENU_PAD * 2).min(img_w);
    let panel_x = (img_w.saturating_sub(panel_w) / 2) as i32;

    // Height = padding + title row + N rows + padding.
    let panel_h = MENU_PAD * 2 + MENU_ROW_H + MENU_ROW_H * rows.len() as u32;
    let panel_top = (img_h.saturating_sub(panel_h) / 2) as i32;
    let rows_top = panel_top + (MENU_PAD + MENU_ROW_H) as i32;

    (panel_x, panel_top, panel_w, panel_h, rows_top, MENU_ROW_H)
}

/// Fill a solid opaque rectangle, clipped to the image.
fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    let (iw, ih) = img.dimensions();
    let x_end = (x + w).min(iw);
    let y_end = (y + h).min(ih);
    if x >= x_end || y >= y_end {
        return;
    }
    let stride = iw as usize * 4;
    let buf = img.as_mut();
    for py in y..y_end {
        let row_off = py as usize * stride;
        for px in x..x_end {
            let i = row_off + px as usize * 4;
            buf[i] = color[0];
            buf[i + 1] = color[1];
            buf[i + 2] = color[2];
            buf[i + 3] = color[3];
        }
    }
}

/// Alpha-composite `color` (with its alpha) over a rectangle, clipped to the
/// image. Used for the translucent panel, drop shadow, and title rule so the
/// menu has depth rather than a flat opaque slab. Menu-only (cold) path, so the
/// per-pixel `/255` is fine.
fn blend_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    let a = color[3] as u32;
    if a == 0 {
        return;
    }
    if a >= 255 {
        return fill_rect(img, x, y, w, h, color);
    }
    let ia = 255 - a;
    let (iw, ih) = img.dimensions();
    let x_end = (x + w).min(iw);
    let y_end = (y + h).min(ih);
    if x >= x_end || y >= y_end {
        return;
    }
    let stride = iw as usize * 4;
    let buf = img.as_mut();
    for py in y..y_end {
        let row_off = py as usize * stride;
        for px in x..x_end {
            let i = row_off + px as usize * 4;
            buf[i] = ((buf[i] as u32 * ia + color[0] as u32 * a) / 255) as u8;
            buf[i + 1] = ((buf[i + 1] as u32 * ia + color[1] as u32 * a) / 255) as u8;
            buf[i + 2] = ((buf[i + 2] as u32 * ia + color[2] as u32 * a) / 255) as u8;
            // Leave alpha opaque — the frame is composited onto an opaque base.
        }
    }
}

/// Draw a row's text as a two-tone "label: value" pair (label in `label_color`,
/// the part after the first ": " in `MENU_VALUE`). Rows without a ": " are drawn
/// entirely in `label_color`.
fn draw_row_text(img: &mut RgbaImage, text: &str, x: i32, y: i32, label_color: [u8; 4]) {
    if let Some(pos) = text.find(": ") {
        let split = pos + 2; // keep ": " with the label
        let label = &text[..split];
        let value = &text[split..];
        draw_text_colored(img, label, x, y, label_color);
        let vx = x + label.chars().count() as i32 * GLYPH_W as i32;
        draw_text_colored(img, value, vx, y, MENU_VALUE);
    } else {
        draw_text_colored(img, text, x, y, label_color);
    }
}

/// Map a screen `(x, y)` to a menu row index, or `None` if it lands outside the
/// panel (used to dismiss on click-away). Returns the geometric row regardless
/// of kind — the caller ignores header rows. Rejects clicks outside the panel
/// horizontally too, so clicking the dimmed photo beside the panel dismisses
/// rather than hitting the row at that height. Mirrors `draw_menu`'s geometry
/// exactly via the shared `menu_geometry`.
pub fn menu_hit_test(
    img_w: u32,
    img_h: u32,
    title: &str,
    rows: &[MenuItem],
    x: i32,
    y: i32,
) -> Option<usize> {
    let n_rows = rows.len();
    if n_rows == 0 {
        return None;
    }
    let (panel_x, _panel_top, panel_w, _panel_h, rows_top, row_h) =
        menu_geometry(img_w, img_h, title, rows);
    if x < panel_x || x >= panel_x + panel_w as i32 || y < rows_top {
        return None;
    }
    let idx = ((y - rows_top) / row_h as i32) as usize;
    (idx < n_rows).then_some(idx)
}

/// Draw the settings menu: a translucent, accent-framed panel centred over a
/// dimmed photo, a title with a hairline rule, and grouped rows. Section headers
/// render in the accent colour; the selected item gets a highlight plus a left
/// accent bar; values (the text after "label: ") render in a softer tint. Uses
/// the same built-in 8×8 bitmap font as the rest of the OSD — no system font.
///
/// Cost: two full-frame dim passes, a few blended rects, and the glyphs. Only
/// called while the menu is open *and* its state changed, so it never runs on
/// the slideshow hot path.
pub fn draw_menu(img: &mut RgbaImage, title: &str, rows: &[MenuItem], selected: usize) {
    let (iw, ih) = img.dimensions();
    let (panel_x_i, panel_top, panel_w, panel_h, rows_top, row_h) =
        menu_geometry(iw, ih, title, rows);
    let panel_x = panel_x_i.max(0) as u32;
    let panel_top_u = panel_top.max(0) as u32;

    // Dim the whole photo so the panel reads clearly (two cheap passes).
    darken_rect(img, 0, 0, iw, ih);
    darken_rect(img, 0, 0, iw, ih);

    // Soft drop shadow (offset down-right) for depth, then the translucent panel.
    blend_rect(
        img,
        panel_x + SHADOW_OFF,
        panel_top_u + SHADOW_OFF,
        panel_w,
        panel_h,
        MENU_SHADOW,
    );
    blend_rect(img, panel_x, panel_top_u, panel_w, panel_h, MENU_BG);

    // 2-px accent frame around the panel.
    draw_border(img, panel_x, panel_top_u, panel_w, panel_h, 2, MENU_BORDER);

    // Title (fake-bold: drawn twice, 1 px apart) + hairline rule beneath it.
    let title_x = (panel_x + MENU_PAD) as i32;
    let title_y = (panel_top_u + MENU_PAD) as i32;
    draw_text_colored(img, title, title_x, title_y, FG);
    draw_text_colored(img, title, title_x + 1, title_y, FG);
    blend_rect(
        img,
        panel_x + MENU_PAD,
        panel_top_u + MENU_PAD + GLYPH_H + 6,
        panel_w.saturating_sub(MENU_PAD * 2),
        2,
        MENU_RULE,
    );

    // Rows.
    let text_inset = ((MENU_ROW_H - GLYPH_H) / 2) as i32;
    for (i, row) in rows.iter().enumerate() {
        let row_y = rows_top + i as i32 * row_h as i32;
        if row_y < 0 {
            continue;
        }
        let text_x = (panel_x + MENU_PAD) as i32;
        let text_y = row_y + text_inset;

        if row.is_header {
            // Headers: accent text, no highlight; sit a touch lower in the slot.
            draw_text_colored(img, row.label, text_x, text_y, MENU_HEADER);
            continue;
        }

        if i == selected {
            fill_rect(
                img,
                panel_x + MENU_PAD / 2,
                row_y as u32,
                panel_w.saturating_sub(MENU_PAD),
                MENU_ROW_H,
                MENU_SEL,
            );
            // Left accent bar marking the selection.
            fill_rect(
                img,
                panel_x + MENU_PAD / 2,
                row_y as u32,
                SEL_BAR_W,
                MENU_ROW_H,
                MENU_SEL_BAR,
            );
            // Selected row: whole line bright for contrast on the highlight.
            draw_row_text(img, row.label, text_x, text_y, FG);
        } else {
            draw_row_text(img, row.label, text_x, text_y, MENU_LABEL);
        }
    }
}

/// Draw a `t`-px-thick rectangular frame (four edges) in `color`.
fn draw_border(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, t: u32, color: [u8; 4]) {
    fill_rect(img, x, y, w, t, color); // top
    fill_rect(img, x, y + h.saturating_sub(t), w, t, color); // bottom
    fill_rect(img, x, y, t, h, color); // left
    fill_rect(img, x + w.saturating_sub(t), y, t, h, color); // right
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn white_img(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]))
    }

    /// Wrap plain labels as non-header menu items for the drawing/hit-test API.
    fn items(labels: &[String]) -> Vec<MenuItem<'_>> {
        labels
            .iter()
            .map(|l| MenuItem {
                label: l.as_str(),
                is_header: false,
            })
            .collect()
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
    fn menu_hit_test_maps_clicks_to_rows() {
        let (img_w, img_h) = (1920u32, 1080u32);
        let title = "Settings";
        let labels: Vec<String> = (0..10).map(|i| format!("row {i}")).collect();
        let rows = items(&labels);
        let (panel_x, _pt, panel_w, _ph, rows_top, row_h) =
            menu_geometry(img_w, img_h, title, &rows);
        let cx = panel_x + panel_w as i32 / 2; // inside the panel horizontally
                                               // Middle of row 3 resolves to index 3.
        let y = rows_top + 3 * row_h as i32 + row_h as i32 / 2;
        assert_eq!(menu_hit_test(img_w, img_h, title, &rows, cx, y), Some(3));
        // Above the first row → no hit (click-away dismiss).
        assert_eq!(
            menu_hit_test(img_w, img_h, title, &rows, cx, rows_top - 5),
            None
        );
        // Below the last row → no hit.
        let below = rows_top + rows.len() as i32 * row_h as i32 + 5;
        assert_eq!(menu_hit_test(img_w, img_h, title, &rows, cx, below), None);
        // Outside the panel horizontally → no hit even at a valid row height.
        assert_eq!(
            menu_hit_test(img_w, img_h, title, &rows, panel_x - 5, y),
            None
        );
        assert_eq!(
            menu_hit_test(img_w, img_h, title, &rows, panel_x + panel_w as i32 + 5, y),
            None
        );
    }

    #[test]
    fn draw_clock_darkens_top_centre_and_does_not_panic() {
        let (w, h) = (320u32, 240u32);
        let mut img = white_img(w, h);
        draw_clock(&mut img, "12:34");
        // Pill sits centred along the top edge; its centre is no longer white.
        let px = img.get_pixel(w / 2, EDGE + (GLYPH_H + PAD * 2) / 2);
        assert!(px[0] < 200, "expected darkened clock pill, got {px:?}");
    }

    #[test]
    fn draw_clock_empty_is_noop() {
        let mut img = white_img(64, 64);
        draw_clock(&mut img, "");
        assert_eq!(img.get_pixel(32, EDGE), &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn draw_menu_does_not_panic() {
        let mut img = white_img(1920, 1080);
        let labels: Vec<String> = (0..9).map(|i| format!("row {i}")).collect();
        let mut rows = items(&labels);
        rows[0].is_header = true; // exercise the header-drawing path
        draw_menu(&mut img, "PicoGallery — Settings", &rows, 2);
    }

    #[test]
    fn draw_menu_does_not_panic_on_small_image() {
        let mut img = white_img(160, 120);
        let labels: Vec<String> = (0..12)
            .map(|i| format!("a rather long row label {i}"))
            .collect();
        draw_menu(&mut img, "Settings", &items(&labels), 5);
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
