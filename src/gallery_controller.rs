//! Gallery mode controller — grid state and dirty-render flag.

use crate::gallery::GalleryGrid;

/// Owns the gallery grid and its repaint flag. Selection restoration on return
/// from fullscreen is driven by the display loop (which tracks the last-viewed
/// queue index), so this type deliberately does not duplicate that state.
pub struct GalleryController {
    pub grid: GalleryGrid,
    pub dirty: bool,
}

impl GalleryController {
    pub fn new(screen_w: u32) -> Self {
        Self {
            grid: GalleryGrid::new(screen_w),
            dirty: true,
        }
    }

    /// Mark the grid for repaint (e.g. on entering gallery mode).
    pub fn enter(&mut self) {
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}
