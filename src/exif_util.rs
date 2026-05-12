/// EXIF orientation reading and correction.
///
/// `read_orientation` and `read_date` parse the EXIF segment once per call and
/// are called together in `renderer::decode_and_scale` so EXIF is never parsed
/// twice for the same photo.
///
/// `apply_orientation_rgba` operates on the *already-scaled* display-sized
/// RgbaImage (~8 MB) rather than the full-resolution DynamicImage (~48–96 MB).
/// This halves peak RAM compared with rotating before scaling; the quality is
/// identical because 90°/180°/270° pixel rotations are lossless at any resolution.
///
/// Note: the `kamadak-exif` package re-exports under the crate name `exif`.
use exif::{In, Reader, Tag, Value};
use image::{imageops, RgbaImage};

// ── EXIF readers ─────────────────────────────────────────────────────────────

/// Read the EXIF Orientation tag (1–8) from raw image bytes.
/// Returns 1 (no transform needed) when EXIF is absent or unreadable.
pub fn read_orientation(bytes: &[u8]) -> u32 {
    let mut cursor = std::io::Cursor::new(bytes);
    let Ok(exif) = Reader::new().read_from_container(&mut cursor) else {
        return 1;
    };
    let Some(field) = exif.get_field(Tag::Orientation, In::PRIMARY) else {
        return 1;
    };
    match &field.value {
        Value::Short(v) => v.first().copied().unwrap_or(1) as u32,
        _ => 1,
    }
}

/// Extract the DateTimeOriginal EXIF tag and return it as "YYYY-MM-DD".
/// Returns `None` when EXIF is absent or the tag is missing.
pub fn read_date(bytes: &[u8]) -> Option<String> {
    let mut cursor = std::io::Cursor::new(bytes);
    let exif = Reader::new().read_from_container(&mut cursor).ok()?;
    let field = exif.get_field(Tag::DateTimeOriginal, In::PRIMARY)?;
    let Value::Ascii(ref v) = field.value else { return None; };
    let raw = v.first()?;
    let s = std::str::from_utf8(raw).ok()?.trim_end_matches('\0');
    // EXIF date: "2023:06:15 14:32:00" → take date part → "2023-06-15"
    Some(s.get(..10)?.replace(':', "-"))
}

// ── Orientation correction (post-scale) ──────────────────────────────────────

/// Apply EXIF orientation correction to a display-sized `RgbaImage`.
///
/// Called *after* scaling (not before) so the large full-resolution image is
/// freed by the scaler before this copy is made.  Peak RAM:
///   scale-then-rotate:  full-res + display-sized  ≈ 48 MB + 8 MB = 56 MB (12 MP)
///   rotate-then-scale:  full-res + full-res-copy  ≈ 48 MB + 48 MB = 96 MB (12 MP)
///
/// | Value | Transform                    |
/// |-------|------------------------------|
/// | 1     | none (upright)               |
/// | 2     | flip horizontal              |
/// | 3     | rotate 180°                  |
/// | 4     | flip vertical                |
/// | 5     | rotate 90° CW + flip horiz   |
/// | 6     | rotate 90° CW (most common)  |
/// | 7     | rotate 270° CW + flip horiz  |
/// | 8     | rotate 270° CW               |
pub fn apply_orientation_rgba(img: RgbaImage, orientation: u32) -> RgbaImage {
    match orientation {
        2 => imageops::flip_horizontal(&img),
        3 => imageops::rotate180(&img),
        4 => imageops::flip_vertical(&img),
        5 => { let r = imageops::rotate90(&img); imageops::flip_horizontal(&r) }
        6 => imageops::rotate90(&img),
        7 => { let r = imageops::rotate270(&img); imageops::flip_horizontal(&r) }
        8 => imageops::rotate270(&img),
        _ => img, // 1 or any unknown value — no transform
    }
}
