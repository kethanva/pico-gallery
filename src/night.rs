//! Night-mode photo adjustment: combined dim + warm tint in one pixel pass.
//!
//! Pi Zero friendly: per-channel 8.8 fixed-point multipliers computed once,
//! then a single multiply-and-shift per channel in the inner loop — no
//! division, no floating point, no per-pixel bounds checks (operates on the
//! raw buffer via `chunks_exact_mut`). One pass over a 1080p frame is a few
//! milliseconds, paid once per slide (never per frame).

use image::RgbaImage;

/// Apply night dimming and warm tint to `img` in place.
///
/// * `dim_percent` (0–90): overall brightness reduction.
/// * `warmth` (0–100): how strongly blue (and slightly green) are reduced,
///   shifting the image toward warm tones. 0 = dim only.
pub fn apply_night(img: &mut RgbaImage, dim_percent: u8, warmth: u8) {
    let dim = dim_percent.min(90) as u32;
    let warmth = warmth.min(100) as u32;

    // Per-channel multipliers in percent. Warmth cuts green a little and
    // blue a lot — at warmth=100: green ×0.80, blue ×0.55.
    let bright = 100 - dim;
    let pct_r = bright;
    let pct_g = bright * (100 - warmth * 20 / 100) / 100;
    let pct_b = bright * (100 - warmth * 45 / 100) / 100;

    // 8.8 fixed point: c' = (c * m) >> 8.
    // Invariant: each multiplier is in 0..=256, where 256 is exact identity
    // passthrough ((c*256)>>8 == c), so the per-channel result always fits in u8.
    let mr = (pct_r * 256 / 100).min(256);
    let mg = (pct_g * 256 / 100).min(256);
    let mb = (pct_b * 256 / 100).min(256);

    if mr == 256 && mg == 256 && mb == 256 {
        return; // no-op settings
    }

    for px in img.as_mut().chunks_exact_mut(4) {
        px[0] = ((px[0] as u32 * mr) >> 8) as u8;
        px[1] = ((px[1] as u32 * mg) >> 8) as u8;
        px[2] = ((px[2] as u32 * mb) >> 8) as u8;
        // px[3] (alpha) untouched.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img_of(r: u8, g: u8, b: u8) -> RgbaImage {
        RgbaImage::from_pixel(4, 4, image::Rgba([r, g, b, 255]))
    }

    #[test]
    fn zero_settings_are_noop() {
        let mut img = img_of(200, 150, 100);
        apply_night(&mut img, 0, 0);
        assert_eq!(img.get_pixel(0, 0).0, [200, 150, 100, 255]);
    }

    #[test]
    fn dim_reduces_all_channels_equally() {
        let mut img = img_of(200, 200, 200);
        apply_night(&mut img, 50, 0);
        let px = img.get_pixel(0, 0).0;
        // 50% dim → ~100 on every channel (fixed-point rounding allowed).
        assert!(px[0].abs_diff(100) <= 2, "r={}", px[0]);
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
        assert_eq!(px[3], 255);
    }

    #[test]
    fn warmth_reduces_blue_more_than_green_more_than_red() {
        let mut img = img_of(200, 200, 200);
        apply_night(&mut img, 0, 100);
        let px = img.get_pixel(0, 0).0;
        assert!(px[0] > px[1], "red should exceed green: {:?}", px);
        assert!(px[1] > px[2], "green should exceed blue: {:?}", px);
    }

    #[test]
    fn clamps_excessive_inputs() {
        let mut img = img_of(255, 255, 255);
        apply_night(&mut img, 255, 255); // out of range — clamped to 90/100
        let px = img.get_pixel(0, 0).0;
        assert!(px[0] > 0, "90% dim must not black out completely");
    }
}
