/// EXIF orientation reading and correction.
///
/// `read_exif` parses the EXIF segment **once** per photo and returns both
/// orientation and capture date.  This replaces the previous two-function API
/// (`read_orientation` + `read_date`) which ran a full
/// `Reader::read_from_container` twice per photo — that allocated the
/// per-tag `Vec<Field>` tree twice and wasted ~300–900 µs per photo on Pi Zero.
///
/// `apply_orientation_rgba` operates on the *already-scaled* display-sized
/// RgbaImage (~8 MB) rather than the full-resolution DynamicImage (~48–96 MB).
/// This halves peak RAM compared with rotating before scaling; the quality is
/// identical because 90°/180°/270° pixel rotations are lossless at any resolution.
///
/// Note: the `kamadak-exif` package re-exports under the crate name `exif`.
use exif::{In, Reader, Tag, Value};
use image::{imageops, RgbaImage};

// ── Single EXIF parse ────────────────────────────────────────────────────────

/// EXIF fields extracted from a single parse pass.
#[derive(Debug, Clone)]
pub struct ExifInfo {
    /// EXIF Orientation tag value (1–8).  Defaults to 1 (no transform needed).
    pub orientation: u32,
    /// `DateTimeOriginal` formatted as "YYYY-MM-DD", if present.
    pub date: Option<String>,
}

impl Default for ExifInfo {
    fn default() -> Self {
        Self { orientation: 1, date: None }
    }
}

/// Parse EXIF once and extract orientation + capture date.
///
/// Returns `ExifInfo::default()` when EXIF is absent or unreadable.
pub fn read_exif(bytes: &[u8]) -> ExifInfo {
    let mut cursor = std::io::Cursor::new(bytes);
    let Ok(exif) = Reader::new().read_from_container(&mut cursor) else {
        return ExifInfo::default();
    };

    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| match &f.value {
            Value::Short(v) => v.first().copied().map(u32::from),
            _ => None,
        })
        .unwrap_or(1);

    let date = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .and_then(|f| {
            let Value::Ascii(ref v) = f.value else { return None; };
            let raw = v.first()?;
            let s = std::str::from_utf8(raw).ok()?.trim_end_matches('\0');
            // EXIF Ascii date: "2023:06:15 14:32:00" → "2023-06-15"
            Some(s.get(..10)?.replace(':', "-"))
        });

    ExifInfo { orientation, date }
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
