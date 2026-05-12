/// EXIF orientation reading and correction.
///
/// Called from `renderer::decode_and_scale` to fix portrait photos taken on phones
/// before the image is scaled, ensuring the scaler sees the correct aspect ratio.
///
/// Note: the `kamadak-exif` package re-exports under the crate name `exif`.
use exif::{In, Reader, Tag, Value};
use image::DynamicImage;

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
    // EXIF Ascii value is Vec<Vec<u8>>; the first component is the string.
    let Value::Ascii(ref v) = field.value else {
        return None;
    };
    let raw = v.first()?;
    let s = std::str::from_utf8(raw).ok()?;
    let s = s.trim_end_matches('\0');
    // EXIF date format: "2023:06:15 14:32:00" — take date part and reformat.
    let date_part = s.get(..10)?; // "2023:06:15"
    Some(date_part.replace(':', "-")) // "2023-06-15"
}

/// Apply EXIF orientation correction to a `DynamicImage` before scaling.
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
pub fn apply_orientation(img: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img, // 1 or any unknown value
    }
}
