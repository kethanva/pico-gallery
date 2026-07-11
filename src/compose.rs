//! Shared image compositing primitives used by gallery, OSD, and slideshow.

use image::{Rgba, RgbaImage};

/// Copy `src` into the centre of `dst`, clipped on every edge. Handles `src`
/// both smaller than `dst` (letterbox — centred with a border) and larger
/// (fill crop — centre region copied). Straight row memcpy, no blending.
pub fn blit_center(dst: &mut RgbaImage, src: &RgbaImage) {
    let (dw, dh) = dst.dimensions();
    let (sw, sh) = src.dimensions();
    let ox = (dw as i32 - sw as i32) / 2;
    let oy = (dh as i32 - sh as i32) / 2;
    let dstride = dw as usize * 4;
    let sstride = sw as usize * 4;
    let dbuf = dst.as_mut();
    let sbuf = src.as_raw();
    for sy in 0..sh as i32 {
        let dy = oy + sy;
        if dy < 0 || dy >= dh as i32 {
            continue;
        }
        let dx0 = ox.max(0);
        let sx0 = dx0 - ox;
        let copy_w = (sw as i32 - sx0).min(dw as i32 - dx0);
        if copy_w <= 0 {
            continue;
        }
        let d = dy as usize * dstride + dx0 as usize * 4;
        let s = sy as usize * sstride + sx0 as usize * 4;
        let n = copy_w as usize * 4;
        dbuf[d..d + n].copy_from_slice(&sbuf[s..s + n]);
    }
}

/// Fill a rectangle with a solid RGBA colour (clipped to image bounds).
///
/// Pre-fills one row of the colour pattern, then `copy_from_slice`s it into
/// each scanline — a memcpy per row instead of a bounds-checked `put_pixel`
/// per pixel (the difference is felt on a Pi Zero for every grid cell).
pub fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    let (iw, ih) = img.dimensions();
    let x_end = (x + w).min(iw);
    let y_end = (y + h).min(ih);
    if x >= x_end || y >= y_end {
        return;
    }
    let row_bytes = (x_end - x) as usize * 4;
    let stride = iw as usize * 4;
    let mut row = vec![0u8; row_bytes];
    for px in row.chunks_exact_mut(4) {
        px.copy_from_slice(&color.0);
    }
    let buf = img.as_mut();
    for py in y..y_end {
        let start = py as usize * stride + x as usize * 4;
        buf[start..start + row_bytes].copy_from_slice(&row);
    }
}

/// Cover-crop `src` into a `size×size` square (single resize + centre crop).
pub fn cover_square(src: RgbaImage, size: u32) -> RgbaImage {
    let size = size.max(1);
    let (sw, sh) = src.dimensions();
    if sw == 0 || sh == 0 {
        return RgbaImage::from_pixel(size, size, Rgba([28, 28, 28, 255]));
    }
    if sw == size && sh == size {
        return src;
    }
    let scale = (size as f32 / sw as f32).max(size as f32 / sh as f32);
    let tw = ((sw as f32 * scale).ceil() as u32).max(1);
    let th = ((sh as f32 * scale).ceil() as u32).max(1);
    let scaled = image::imageops::resize(&src, tw, th, image::imageops::FilterType::Triangle);
    let ox = scaled.width().saturating_sub(size) / 2;
    let oy = scaled.height().saturating_sub(size) / 2;
    image::imageops::crop_imm(&scaled, ox, oy, size, size).to_image()
}

/// Blit a rectangular region from `src` into `dst` at `(x, y)`, optionally
/// skipping `src_y0` source rows (scroll clipping).
pub fn blit_clipped(
    dst: &mut RgbaImage,
    src: &RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    src_y0: u32,
) {
    if w == 0 || h == 0 {
        return;
    }
    let (sw, sh) = src.dimensions();
    let (iw, ih) = dst.dimensions();
    let dstride = dst.width() as usize * 4;
    let sstride = sw as usize * 4;
    let dbuf = dst.as_mut();
    let sbuf = src.as_raw();
    let copy_w = w.min(sw);
    for row in 0..h {
        let dy = y + row;
        if dy >= ih {
            break;
        }
        let sy = src_y0 + row;
        if sy >= sh {
            continue;
        }
        let cols = copy_w.min(iw.saturating_sub(x));
        if cols == 0 {
            continue;
        }
        let d = dy as usize * dstride + x as usize * 4;
        let s = sy as usize * sstride;
        dbuf[d..d + cols as usize * 4].copy_from_slice(&sbuf[s..s + cols as usize * 4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_center_centres_smaller_source_without_overflow() {
        let mut dst = RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 255]));
        let src = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
        blit_center(&mut dst, &src);
        assert_eq!(*dst.get_pixel(3, 3), Rgba([255, 0, 0, 255]));
        assert_eq!(*dst.get_pixel(0, 0), Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn blit_center_crops_larger_source_without_panicking() {
        let mut dst = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]));
        let src = RgbaImage::from_pixel(10, 10, Rgba([1, 2, 3, 255]));
        blit_center(&mut dst, &src);
        assert_eq!(*dst.get_pixel(0, 0), Rgba([1, 2, 3, 255]));
    }

    #[test]
    fn cover_square_produces_exact_size() {
        let src = RgbaImage::from_pixel(20, 10, Rgba([9, 9, 9, 255]));
        let out = cover_square(src, 8);
        assert_eq!(out.dimensions(), (8, 8));
    }
}
