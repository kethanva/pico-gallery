//! Gallery-only EXIF thumbnail preference.
//!
//! Filesystem plugins always return the full original JPEG. The slideshow
//! engine may substitute an embedded IFD1 JPEG thumbnail **only** on the
//! gallery thumb path (`fetch_photo_thumb`), never for fullscreen fetches —
//! small LCDs (e.g. 480×320) pass display size into `get_photo_bytes`, and
//! treating that as a "thumb request" would upscale a 160×120 stub.
//!
//! When an EXIF thumb is used, the primary IFD0 Orientation is re-injected
//! into the returned JPEG so `decode_thumbnail`'s `read_exif` still rotates
//! portrait photos correctly.

use exif::experimental::Writer;
use exif::{Field, In, Reader, Tag, Value};
use std::io::Cursor;

/// How much of a JPEG's head is worth reading to find an IFD1 thumbnail.
/// EXIF APP1 sits immediately after SOI; 256 KiB covers typical camera and
/// phone encoders with room for a maker-note block.
pub const EXIF_HEAD_SCAN_BYTES: usize = 256 * 1024;

/// Try to satisfy a `cell_px` gallery cell from `head` alone — the first
/// [`EXIF_HEAD_SCAN_BYTES`] of a JPEG. `None` means "read the whole file".
/// Reuses the same usability rules as [`prefer_gallery_exif_thumb`], so a
/// 160×120 stub is never promoted onto a large grid.
pub fn exif_thumb_from_head(head: &[u8], cell_px: u32) -> Option<Vec<u8>> {
    let (thumb, orientation, ifd1_edge) = extract_exif_jpeg_thumbnail_with_orientation(head)?;
    let need = cell_px.max(1);
    if !thumb_looks_usable(&thumb, need, ifd1_edge) {
        return None;
    }
    jpeg_with_orientation(&thumb, orientation)
}

/// Prefer an embedded EXIF JPEG thumbnail for a gallery cell of `cell_px`
/// pixels, preserving primary-image orientation.
///
/// Call only from the gallery thumb fetch path — never from fullscreen
/// `fetch_photo`.
pub fn prefer_gallery_exif_thumb(full: Vec<u8>, cell_px: u32) -> Vec<u8> {
    let need = cell_px.max(1);
    let Some((thumb, orientation, ifd1_edge)) = extract_exif_jpeg_thumbnail_with_orientation(&full)
    else {
        return full;
    };
    if !thumb_looks_usable(&thumb, need, ifd1_edge) {
        return full;
    }
    jpeg_with_orientation(&thumb, orientation).unwrap_or(full)
}

/// Extract IFD1 JPEG thumb bytes, primary Orientation (1–8), and optional
/// IFD1 max edge (ImageWidth/ImageLength) when present.
pub fn extract_exif_jpeg_thumbnail_with_orientation(
    bytes: &[u8],
) -> Option<(Vec<u8>, u32, Option<u32>)> {
    let mut cursor = Cursor::new(bytes);
    let exif = Reader::new().read_from_container(&mut cursor).ok()?;

    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(1);

    let offset = exif
        .get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let len = exif
        .get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let buf = exif.buf();
    let end = offset.checked_add(len)?;
    if end > buf.len() || len < 3 {
        return None;
    }
    let thumb = &buf[offset..end];
    if thumb[0] != 0xFF || thumb[1] != 0xD8 || thumb[2] != 0xFF {
        return None;
    }

    let ifd1_edge = match (
        exif.get_field(Tag::ImageWidth, In::THUMBNAIL)
            .and_then(|f| f.value.get_uint(0)),
        exif.get_field(Tag::ImageLength, In::THUMBNAIL)
            .and_then(|f| f.value.get_uint(0)),
    ) {
        (Some(w), Some(h)) => Some(w.max(h)),
        _ => None,
    };

    Some((thumb.to_vec(), orientation, ifd1_edge))
}

/// Backward-compatible extract without orientation.
pub fn extract_exif_jpeg_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_exif_jpeg_thumbnail_with_orientation(bytes).map(|(t, _, _)| t)
}

fn thumb_looks_usable(thumb: &[u8], need_edge: u32, ifd1_edge: Option<u32>) -> bool {
    // EXIF thumbs are typically 10–80 KB. Reject stubs and oversized blobs.
    if thumb.len() < 1_024 || thumb.len() > 512 * 1024 {
        return false;
    }
    if let Some(edge) = ifd1_edge {
        // Allow mild upscale (EXIF thumbs are often ~160px); refuse stubs and
        // refuse when the cell is much larger than the embedded thumb.
        if edge < 64 {
            return false;
        }
        // need <= 2× embedded edge (e.g. 160px thumb OK up to ~320px cell)
        return need_edge <= edge.saturating_mul(2);
    }
    // Without IFD1 dimensions, only use for modest gallery cells so a
    // typical 160×120 stub is never forced onto a near-fullscreen grid.
    need_edge <= 256
}

/// Insert a minimal EXIF APP1 with Orientation after SOI so downstream
/// `read_exif` applies the primary image's rotation to the IFD1 bitstream.
fn jpeg_with_orientation(jpeg: &[u8], orientation: u32) -> Option<Vec<u8>> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }
    let orientation = orientation.clamp(1, 8);
    if orientation == 1 {
        return Some(jpeg.to_vec());
    }

    let field = Field {
        tag: Tag::Orientation,
        ifd_num: In::PRIMARY,
        value: Value::Short(vec![orientation as u16]),
    };
    let mut writer = Writer::new();
    writer.push_field(&field);
    let mut tiff = Cursor::new(Vec::new());
    writer.write(&mut tiff, true).ok()?;
    let tiff = tiff.into_inner();

    // APP1: marker + length (includes length bytes) + "Exif\0\0" + TIFF
    let payload_len = 2u32 + 6 + tiff.len() as u32;
    if payload_len > u16::MAX as u32 {
        return None;
    }
    let mut app1 = Vec::with_capacity(payload_len as usize + 2);
    app1.extend_from_slice(&[0xFF, 0xE1]);
    app1.extend_from_slice(&(payload_len as u16).to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);

    let mut out = Vec::with_capacity(2 + app1.len() + jpeg.len().saturating_sub(2));
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_thumb_from_head_declines_when_absent() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        assert!(exif_thumb_from_head(&jpeg, 140).is_none());
    }

    #[test]
    fn prefer_gallery_passes_through_without_exif_thumb() {
        let full = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        let out = prefer_gallery_exif_thumb(full.clone(), 140);
        assert_eq!(out, full);
    }

    #[test]
    fn prefer_gallery_refuses_large_cell_without_known_dims() {
        let full = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        let out = prefer_gallery_exif_thumb(full.clone(), 400);
        assert_eq!(out, full);
    }

    #[test]
    fn thumb_usable_respects_ifd1_edge() {
        let fake = vec![0u8; 2048];
        assert!(thumb_looks_usable(&fake, 140, Some(160)));
        assert!(!thumb_looks_usable(&fake, 400, Some(160))); // would 2.5× upscale
        assert!(thumb_looks_usable(&fake, 200, None)); // modest cell, unknown dims
        assert!(!thumb_looks_usable(&fake, 400, None));
    }

    #[test]
    fn jpeg_with_orientation_is_noop_for_normal() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let out = jpeg_with_orientation(&jpeg, 1).unwrap();
        assert_eq!(out, jpeg);
    }

    #[test]
    fn jpeg_with_orientation_injects_readable_exif() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let out = jpeg_with_orientation(&jpeg, 6).unwrap();
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF, 0xE1]));
        let mut cursor = Cursor::new(&out);
        let exif = Reader::new().read_from_container(&mut cursor).unwrap();
        let orient = exif
            .get_field(Tag::Orientation, In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .unwrap();
        assert_eq!(orient, 6);
    }
}
