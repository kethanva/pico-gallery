//! Gallery mode controller — selection, paging, and dirty-render state.

use crate::gallery::GalleryGrid;
use crate::mode::Mode;

/// Owns gallery grid state and the selection to restore when returning from
/// fullscreen. Controllers do not know plugin names.
pub struct GalleryController {
    pub grid: GalleryGrid,
    /// Index to restore when leaving fullscreen back to the grid.
    pub restore_selected: Option<usize>,
    pub dirty: bool,
}

impl GalleryController {
    pub fn new(screen_w: u32) -> Self {
        Self {
            grid: GalleryGrid::new(screen_w),
            restore_selected: None,
            dirty: true,
        }
    }

    pub fn enter(&mut self, count: usize) {
        if let Some(idx) = self.restore_selected.take() {
            self.grid.set_selected(idx, count);
        }
        self.dirty = true;
    }

    pub fn open_fullscreen(&mut self) -> Mode {
        self.restore_selected = Some(self.grid.selected);
        Mode::Fullscreen
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn clear_queue_ui(&mut self) {
        self.grid.clear();
        self.restore_selected = None;
        self.dirty = true;
    }
}
